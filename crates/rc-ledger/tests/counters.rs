use rc_ledger::counters::{
    bump, quota_stopped, read_all, set_one, METRICS, QUOTA_STOPPED_METRIC, SUBJECT_KINDS,
};
use rc_store::Root;

#[test]
fn counters_are_one_narrow_row_per_subject_metric() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    bump(root.ledger_mut(), "device", "dev-1", "ingest_turns_accepted", 12, 100).unwrap();
    bump(root.ledger_mut(), "device", "dev-1", "ingest_turns_accepted", 3, 110).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].count, 15);
    assert_eq!(rows[0].last_at_ms, 110);
}

#[test]
fn unknown_subject_kind_or_metric_is_rejected_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    assert!(bump(root.ledger_mut(), "user", "u", "ingest_turns_accepted", 1, 1).is_err());
    assert!(bump(root.ledger_mut(), "device", "dev-1", "body_text", 1, 1).is_err());
}

#[test]
fn table_columns_cannot_hold_body() {
    // 架构 §10 生命周期侧第③条：这张表逐行断言“没有一列能装下正文”，
    // 断的是 schema 不是代码。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let cols = rc_ledger::counters::assert_no_body_shape(root.ledger()).unwrap();
    assert_eq!(cols, vec!["subject_kind", "subject_id", "metric", "count", "last_at_ms"]);
    assert_eq!(SUBJECT_KINDS.len(), 4);
    assert_eq!(METRICS.len(), 27);
    assert!(METRICS.contains(&QUOTA_STOPPED_METRIC), "新指标不在白名单里");
}

/// S1b-C2b：`ownership_conflicts` 是可 bump、可读回来的一格（白名单加了名字还不够，
/// `check` 那一步、`bump` 的累加与 `read_all` 的列形状都要真的走得通）。
#[test]
fn ownership_conflicts_is_bumpable_and_readable() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    assert!(METRICS.contains(&"ownership_conflicts"), "新指标不在白名单里");
    bump(root.ledger_mut(), "maintenance", "ownership", "ownership_conflicts", 2, 100).unwrap();
    bump(root.ledger_mut(), "maintenance", "ownership", "ownership_conflicts", 1, 110).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    let row = rows
        .iter()
        .find(|r| r.metric == "ownership_conflicts")
        .expect("ownership_conflicts 没读回来");
    assert_eq!(row.subject_kind, "maintenance");
    assert_eq!(row.subject_id, "ownership");
    assert_eq!(row.count, 3, "同一主语 + 同一 metric 必须是累加");
    assert_eq!(row.last_at_ms, 110);
    // 其它指标的语义不动：白名单只多一格。
    assert!(bump(root.ledger_mut(), "maintenance", "ownership", "owner_conflict", 1, 1).is_err(),
        "单数形式的拼写不许悄悄混进白名单");
}

#[test]
fn ownership_unresolved_is_bumpable_and_readable() {
    // S1b-C4b2a：回填的未决数与 `ownership_conflicts` 同主语（maintenance:ownership），
    // 但它是**另一个** metric：一次回填 bump 一次，白名单里必须两格都在。
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    assert!(METRICS.contains(&"ownership_unresolved"), "新指标不在白名单里");
    bump(root.ledger_mut(), "maintenance", "ownership", "ownership_unresolved", 3, 100).unwrap();
    bump(root.ledger_mut(), "maintenance", "ownership", "ownership_conflicts", 1, 100).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    let unresolved = rows
        .iter()
        .find(|r| r.metric == "ownership_unresolved")
        .expect("ownership_unresolved 没读回来");
    assert_eq!(unresolved.count, 3);
    assert_eq!(unresolved.subject_kind, "maintenance");
    assert_eq!(unresolved.subject_id, "ownership");
    assert_eq!(rows.len(), 2, "两个 metric 必须各占一行：{rows:?}");
}

/// S3a：`quota_stopped` 读的是"最近一次运行是否触顶"，**不是**生命周期累计。
/// 三档各一条：无行 ⇒ `None`、写过 0 ⇒ `Some(false)`、写过 1 ⇒ `Some(true)`；
/// 再叠两条阴性：覆盖写回 0 必须立刻变 `false`（`set_one` 语义），而 88 次累计
/// `llm_calls` 与其它主语上的同名 metric 都不许把这一格点亮。
#[test]
fn quota_stopped_reads_the_latest_run_not_the_lifetime_sum() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    assert_eq!(quota_stopped(&read_all(root.ledger()).unwrap()), None,
        "一行都没有 ⇒ None（还不是 false：这一格区分'没有运行记录'与'运行过且没触顶'）");
    set_one(root.ledger(), "llm_job", "naming", QUOTA_STOPPED_METRIC, 0, 100).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    assert_eq!(quota_stopped(&rows), Some(false), "最近一次运行没触顶：{rows:?}");
    set_one(root.ledger(), "llm_job", "naming", QUOTA_STOPPED_METRIC, 1, 110).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    assert_eq!(quota_stopped(&rows), Some(true), "最近一次运行触顶：{rows:?}");
    set_one(root.ledger(), "llm_job", "naming", QUOTA_STOPPED_METRIC, 0, 120).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    assert_eq!(quota_stopped(&rows), Some(false),
        "覆盖写回 0 之后必须读 false——累加语义会把 1 永远留住：{rows:?}");
    // 阴性一：生命周期累计（旧口径的真凶）不参与这一格。
    bump(root.ledger(), "llm_job", "naming", "llm_calls", 88, 130).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    assert_eq!(quota_stopped(&rows), Some(false), "88 次累计不许冒充本轮触顶：{rows:?}");
    // 阴性二：同名 metric 出现在非 `llm_job` 主语上不算（这一格的主语是抽取作业）。
    set_one(root.ledger(), "llm_job", "naming", QUOTA_STOPPED_METRIC, 0, 140).unwrap();
    set_one(root.ledger(), "device", "dev-1", QUOTA_STOPPED_METRIC, 1, 150).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    assert_eq!(quota_stopped(&rows), Some(false),
        "device 主语上的同名 metric 不许点亮 quota_stopped：{rows:?}");
    // 正面：三个 pass 各写各的主语，任一为 1 ⇒ 最近一次运行里有一次触顶。
    set_one(root.ledger(), "llm_job", "event", QUOTA_STOPPED_METRIC, 1, 160).unwrap();
    let rows = read_all(root.ledger()).unwrap();
    assert_eq!(quota_stopped(&rows), Some(true),
        "任一 llm_job 运行记录触顶 ⇒ Some(true)：{rows:?}");
}

#[test]
fn unreadable_row_is_reported_not_dropped() {
    // 架构 §5 那条"不许静默截断"落到读数面上：`read_all` 曾把读失败的行 `filter_map(|x| x.ok())`
    // 丢掉并返回 Ok ⇒ doctor 会少报一个计数器而不自知。SQLite 的动态类型让"整数列出 TEXT 值"
    // 可以被造出来（INTEGER 亲和性只转换纯数字串），用它当注入点。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    bump(root.ledger(), "device", "dev-1", "tool_call", 1, 10).unwrap();
    let n = root
        .ledger()
        .conn()
        .execute(
            "INSERT INTO usage_counter(subject_kind, subject_id, metric, count, last_at_ms)
             VALUES('device','dev-2','tool_call','not-a-number',20)",
            [],
        )
        .unwrap();
    assert_eq!(n, 1, "坏行没写进去 ⇒ 这条用例什么都没测");
    assert!(read_all(root.ledger()).is_err(), "读不动的行被静默丢掉了");
}
