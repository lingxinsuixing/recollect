//! `llm_state` 的读写（A47 节 4 第 7 条）。夹具只取最小形状：本文件三组用例都只要账本句柄，
//! 不需要 `water.rs` 那套 `Rc`+`Deref` 大机器。`TempDir` 跟着返回值一起挂着保活——
//! 否则库建完目录就被删（理由见 `water.rs` 顶部那段注释）。
use rc_ledger::llm_state::{self, Row};

fn ledger() -> (tempfile::TempDir, rc_store::Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    (dir, root)
}

fn row(machine: &str, slug: &str, due: Option<i64>, ok: u32, failures: usize, written_at_ms: i64) -> Row {
    Row {
        machine: machine.into(),
        slug: slug.into(),
        probe_due_at_ms: due,
        probe_ok: ok,
        probe_failures: failures,
        written_at_ms,
    }
}

#[test]
fn a_written_row_reads_back_field_for_field() {
    // 往返逐字：六个字段一个不许变。尤其 `probe_due_at_ms` 的 None 不许读成 Some(0)——
    // 那是"没有待做探测"与"随时可探测"的分别（段 A 的 persisted() 正是靠这个区分写的）。
    let (_dir, root) = ledger();
    let l = root.ledger();
    let want = row("device-01", "kimi", None, 2, 3, 1_700_000_000_000);
    llm_state::write(l, &want).unwrap();
    let got = llm_state::read(l, "device-01").unwrap().expect("刚写入的行按同一 machine 就该读得回");
    assert_eq!(got.probe_due_at_ms, None, "None 被读成了别的值（Some(0)？）——空/零之分别被抹掉了");
    assert_eq!(got, want);
    // 对偶形状：到期时刻有值时同样要逐字回来。
    let timed = row("device-02", "deepseek", Some(9_000), 0, 1, 5_000);
    llm_state::write(l, &timed).unwrap();
    assert_eq!(llm_state::read(l, "device-02").unwrap().unwrap(), timed);
}

#[test]
fn a_row_written_for_one_machine_is_not_readable_for_another() {
    // 按机器取：写 A 机器一行 ⇒ 读 B 机器 ⇒ None。这条就是作废规则第 1 条在存储层的形状。
    // 阳性对照在同一条连接上：读 A 自己必须命中，否则"is_none"可能只是"整表都读不出来"。
    let (_dir, root) = ledger();
    let l = root.ledger();
    llm_state::write(l, &row("device-A", "backup", Some(9_000), 1, 0, 5_000)).unwrap();
    assert!(llm_state::read(l, "device-B").unwrap().is_none(), "别的机器读到了 A 的行");
    assert!(llm_state::read(l, "device-A").unwrap().is_some(), "阳性对照：写入侧本身没落地");
}

#[test]
fn rewriting_a_machine_updates_in_place_and_keeps_one_row() {
    // 覆盖写不增行：同 machine 写两次 ⇒ count(*)=1，且【第二次】的字段值读得出来
    // （防"ON CONFLICT 那段其实没生效、第一次的值留在表里"）。
    let (_dir, root) = ledger();
    let l = root.ledger();
    llm_state::write(l, &row("device-A", "old-slug", None, 0, 0, 1_000)).unwrap();
    let second = row("device-A", "new-slug", Some(4_000), 3, 7, 2_000);
    llm_state::write(l, &second).unwrap();
    let n: i64 = l.conn()
        .query_row("SELECT count(*) FROM llm_state WHERE machine=?1",
                   rusqlite::params!["device-A"], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "同 machine 两次写入变成了两行");
    assert_eq!(llm_state::read(l, "device-A").unwrap().unwrap(), second,
               "读回来的是第一次的值 ⇒ ON CONFLICT DO UPDATE 没生效");
}
