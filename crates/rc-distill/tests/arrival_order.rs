//! D28：四个 pass 的会话清单按**最新到达钟**（`MAX(turns.ingested_at_ms)`）倒序选批，
//! 新导入的会话先被咬；同刻按 `(session_key, device_id)` 排，全序确定；老会话在新会话
//! 落终态后仍会被处理。
//!
//! 夹具故意让旧会话的键在字典序上在前（`a-old` < `z-new`），这样旧排序会先选 `a-old`，
//! 四条选批断言在未改实现上必红。会话内顺序仍按 `created_at_ms, turn_uid`。

use rc_contract::{normalize, parse};
use rc_distill::{
    select_event_batch, select_fact_batch, select_naming_batch, select_state_batch, Batch, FactFamily,
};
use rc_ledger::insert_batch;
use rc_ledger::pass_state::{mark_terminal, PassKey, PassState};
use rc_store::Root;
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;

struct Fixture {
    _dir: tempfile::TempDir,
    root: Root,
    /// `a-old` 的 25 条 uid，按 `created_at_ms, turn_uid` 顺序。
    old: Vec<String>,
    /// `z-new` 的 5 条 uid，按 `created_at_ms, turn_uid` 顺序。
    new: Vec<String>,
}

fn fixture() -> Fixture {
    fixture_with_order(false)
}

/// 两个会话：`a-old` 25 轮、到达早（ingested 100_000+i）；`z-new` 5 轮、到达晚
/// （ingested 900_000+i）。`created_at_ms` 各自单调，互不重叠，所以会话内顺序可预测。
///
/// `new_first=true` 时把 `z-new` 放在 JSON 前面（先入库）——专门让"插入序"与
/// "`session_key` 升序"相反；同刻 tie-break 用例靠这个才有判别力。
fn fixture_with_order(new_first: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let old_session = json!({
        "session_key": "a-old",
        "device_id": "dev-a",
        "source_agent": "agent-a",
        "turns": (0..25).map(|i| json!({
            "role": if i % 2 == 0 { "human" } else { "agent" },
            "body": format!("老会话第 {i} 句，讲的是项目进度"),
            "created_at_ms": 10_000 + i as i64,
        })).collect::<Vec<_>>()
    });
    let new_session = json!({
        "session_key": "z-new",
        "device_id": "dev-z",
        "source_agent": "agent-z",
        "turns": (0..5).map(|i| json!({
            "role": if i % 2 == 0 { "human" } else { "agent" },
            "body": format!("新会话第 {i} 句，讲的是项目进度"),
            "created_at_ms": 30_000 + i as i64,
        })).collect::<Vec<_>>()
    });
    let sessions = if new_first {
        vec![new_session, old_session]
    } else {
        vec![old_session, new_session]
    };
    let doc = json!({ "format_version": 1, "sessions": sessions });
    let file = parse(doc.to_string().as_bytes()).expect("夹具必须过契约");
    let rep = normalize(file, NOW);
    let mut old = Vec::new();
    let mut new = Vec::new();
    for a in &rep.accepted {
        let ingested = match a.session.session_key.as_str() {
            "a-old" => 100_000 + (a.turn.created_at_ms - 10_000),
            "z-new" => 900_000 + (a.turn.created_at_ms - 30_000),
            other => panic!("夹具没有这个会话：{other}"),
        };
        insert_batch(
            root.transcript_mut(),
            &a.session,
            std::slice::from_ref(&a.turn),
            ingested,
        )
        .unwrap();
        match a.session.session_key.as_str() {
            "a-old" => old.push(a.turn.turn_uid.clone()),
            _ => new.push(a.turn.turn_uid.clone()),
        }
    }
    assert_eq!(old.len(), 25, "a-old 必须真落下 25 轮");
    assert_eq!(new.len(), 5, "z-new 必须真落下 5 轮");
    Fixture { _dir: dir, root, old, new }
}

fn uids_of(batch: &Batch) -> Vec<String> {
    batch.rows.iter().map(|r| r.turn_uid.clone()).collect()
}

fn naming(root: &Root, limit: usize, now: i64) -> Option<Batch> {
    select_naming_batch(root.transcript(), root.ledger(), root.memory(), limit, now)
        .unwrap()
        .map(|p| p.batch)
}

fn state(root: &Root, limit: usize, now: i64) -> Option<Batch> {
    select_state_batch(root.transcript(), root.ledger(), root.memory(), limit, now)
        .unwrap()
        .map(|p| p.batch)
}

fn event(root: &Root, limit: usize, now: i64) -> Option<Batch> {
    select_event_batch(root.transcript(), root.ledger(), root.memory(), limit, now)
        .unwrap()
        .map(|p| p.batch)
}

fn fact(root: &Root, limit: usize, now: i64) -> Option<Batch> {
    select_fact_batch(root.transcript(), root.ledger(), root.memory(), limit, FactFamily::Promise, now)
        .unwrap()
        .map(|p| p.batch)
}

fn mark_new_terminal(f: &Fixture, pass_id: &str, family: &str) {
    for uid in &f.new {
        mark_terminal(
            f.root.ledger(),
            PassKey { turn_uid: uid, pass_id, family },
            PassState::Done,
            None,
            NOW,
        )
        .unwrap();
    }
}

#[test]
fn naming_selects_the_latest_arrival_first() {
    let f = fixture();
    let b = naming(&f.root, 20, NOW).expect("z-new 的 5 轮都还没跑，必有批");
    assert_eq!(b.session_key, "z-new", "新到达的会话必须先被选到");
    assert_eq!(uids_of(&b), f.new, "批内按 created_at_ms, turn_uid");
}

#[test]
fn state_selects_the_latest_arrival_first() {
    let f = fixture();
    let b = state(&f.root, 20, NOW).expect("z-new 的 5 轮都还没跑，必有批");
    assert_eq!(b.session_key, "z-new", "新到达的会话必须先被选到");
    assert_eq!(uids_of(&b), f.new, "批内按 created_at_ms, turn_uid");
}

#[test]
fn event_selects_the_latest_arrival_first() {
    let f = fixture();
    let b = event(&f.root, 20, NOW).expect("z-new 的 5 轮都还没跑，必有批");
    assert_eq!(b.session_key, "z-new", "新到达的会话必须先被选到");
    assert_eq!(uids_of(&b), f.new, "批内按 created_at_ms, turn_uid");
}

#[test]
fn fact_selects_the_latest_arrival_first() {
    let f = fixture();
    let b = fact(&f.root, 20, NOW).expect("z-new 的 5 轮都还没跑，必有批");
    assert_eq!(b.session_key, "z-new", "新到达的会话必须先被选到");
    assert_eq!(uids_of(&b), f.new, "批内按 created_at_ms, turn_uid");
}

/// 同一库、同一状态：`now` 与 `now+1` 选出的会话与轮次逐字相同（tie-break 的全序，
/// 不依赖 `now`、随机数或进程内游标）。
#[test]
fn same_state_selects_the_same_batch_at_now_and_now_plus_one() {
    let f = fixture();
    type Pick = fn(&Root, usize, i64) -> Option<Batch>;
    let picks: [(&str, Pick); 4] = [
        ("naming", naming),
        ("state", state),
        ("event", event),
        ("fact", fact),
    ];
    for (pass, pick) in picks {
        let first = pick(&f.root, 20, NOW).unwrap_or_else(|| panic!("{pass}: 第一次选批不该空"));
        let second = pick(&f.root, 20, NOW + 1).unwrap_or_else(|| panic!("{pass}: 第二次选批不该空"));
        assert_eq!(first.session_key, second.session_key, "{pass}: 同一状态同一会话");
        assert_eq!(uids_of(&first), uids_of(&second), "{pass}: 同一状态同一批轮次");
    }
}

/// 同刻到达（两会话 `MAX(ingested_at_ms)` 相等）：tie-break 是 `session_key, device_id` 升序，
/// 所以字典序在前的 `a-old` 先被选。这个方向别写反——它保护的是"同刻也确定"，
/// 不是"同刻也新数据先"。
#[test]
fn equal_arrival_falls_back_to_the_ascending_session_key() {
    // z-new 先入库：没有 tie-break 时 SQLite 的扫描序会先给 z-new；有 tie-break 才是 a-old。
    let f = fixture_with_order(true);
    f.root
        .transcript()
        .conn()
        .execute("UPDATE turns SET ingested_at_ms=900_000", [])
        .unwrap();
    // 四个 pass 共用同一段 tie-break；一次循环把四份复制都钉住。
    type Pick = fn(&Root, usize, i64) -> Option<Batch>;
    let picks: [(&str, Pick); 4] = [
        ("naming", naming),
        ("state", state),
        ("event", event),
        ("fact", fact),
    ];
    for (pass, pick) in picks {
        let b = pick(&f.root, 20, NOW).unwrap_or_else(|| panic!("{pass}: 同刻时不该空"));
        assert_eq!(b.session_key, "a-old", "{pass}: 同刻必须按 session_key 升序");
        assert_eq!(uids_of(&b), f.old[..20].to_vec(), "{pass}: 批内顺序不变");
    }
}

#[test]
fn naming_drains_the_old_session_once_the_new_one_is_terminal() {
    let f = fixture();
    mark_new_terminal(&f, "naming", "");
    let b = naming(&f.root, 20, NOW).expect("a-old 还有 25 轮可选");
    assert_eq!(b.session_key, "a-old", "新会话落终态后必须轮到老账");
    assert_eq!(uids_of(&b), f.old[..20].to_vec(), "老账按自己的 created_at 取前 20 轮");
}

#[test]
fn state_drains_the_old_session_once_the_new_one_is_terminal() {
    let f = fixture();
    mark_new_terminal(&f, "state", "");
    let b = state(&f.root, 20, NOW).expect("a-old 还有 25 轮可选");
    assert_eq!(b.session_key, "a-old", "新会话落终态后必须轮到老账");
    assert_eq!(uids_of(&b), f.old[..20].to_vec(), "老账按自己的 created_at 取前 20 轮");
}

#[test]
fn event_drains_the_old_session_once_the_new_one_is_terminal() {
    let f = fixture();
    mark_new_terminal(&f, "event", "");
    let b = event(&f.root, 20, NOW).expect("a-old 还有 25 轮可选");
    assert_eq!(b.session_key, "a-old", "新会话落终态后必须轮到老账");
    assert_eq!(uids_of(&b), f.old[..20].to_vec(), "老账按自己的 created_at 取前 20 轮");
}

#[test]
fn fact_drains_the_old_session_once_the_new_one_is_terminal() {
    let f = fixture();
    mark_new_terminal(&f, "fact", FactFamily::Promise.slug());
    let b = fact(&f.root, 20, NOW).expect("a-old 还有 25 轮可选");
    assert_eq!(b.session_key, "a-old", "新会话落终态后必须轮到老账");
    assert_eq!(uids_of(&b), f.old[..20].to_vec(), "老账按自己的 created_at 取前 20 轮");
}
