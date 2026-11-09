//! P2：`pass_id` 的 CHECK 加宽到含 `state`/`lesson` 时，**旧库**必须被 `rc-store` 的
//! `widen_closed_set_check` 重建跟上。
//!
//! 为什么这条用例必须有：fresh 库由 `apply` 直接按 canonical 建表，永远测不到迁移那一支；
//! 而真实存在的库（本机实验目录、中心数据目录、测试机）都是旧形状。少了这一条，
//! "旧库第一次跑 `distill --pass state` 会不会撞 CHECK"只能靠上线时踩。
//!
//! 三条断言：① 表退回旧 CHECK 后重新开库 ⇒ 原文已含 `'state'`；② 旧行一行不少地活过重建
//! （含索引重建）；③ 加宽是真的加宽——`pass_id='state'` 现在写得进去。

use rc_store::Root;

fn table_sql(conn: &rusqlite::Connection, name: &str) -> String {
    conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

fn index_names(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name=?1 ORDER BY name")
        .unwrap();
    st.query_map([table], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

/// 把一张表按给定 CHECK 重建成"旧形状"（只换约束文本，列不动），并保留原有行。
fn regress_to(conn: &rusqlite::Connection, table: &str, old_check: &str) {
    let current = table_sql(conn, table);
    let widened = match table {
        // 波三修 1：canonical 再含 `'lesson'`（第 6 pass 的终态行）。
        "extract_pass_state" => "CHECK(pass_id IN ('naming','fact','event','time','state','lesson'))",
        _ => "CHECK(pass_id IN ('naming','fact','event','time','rows','state'))",
    };
    assert!(current.contains(widened), "夹具前提破了：canonical 里没有那一条 CHECK");
    let old = current.replace(widened, old_check);
    let staged = format!("{table}__old");
    conn.execute_batch(
        // `sqlite_master` 存的原文不含 `IF NOT EXISTS`（SQLite 落库时剥掉），
        // 所以这里换的是**读回来那一份**的表名，而不是 canonical 的写法。
        &old.replace(
            &format!("CREATE TABLE {table}("),
            &format!("CREATE TABLE {staged}("),
        ),
    )
    .unwrap();
    // 夹具要的是"旧约束 + 同样的行"，所以换表前先把数据搬过去。
    let columns: Vec<String> = {
        let mut st = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"))
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let list = columns.join(",");
    conn.execute_batch(&format!(
        "INSERT INTO {staged}({list}) SELECT {list} FROM {table}"
    ))
    .unwrap();
    conn.execute_batch(&format!("DROP TABLE {table}")).unwrap();
    conn.execute_batch(&format!("ALTER TABLE {staged} RENAME TO {table}")).unwrap();
}

const OLD_LEDGER_CHECK: &str = "CHECK(pass_id IN ('naming','fact','event','time'))";

#[test]
fn an_old_ledger_gets_its_pass_id_check_widened_and_keeps_its_rows() {
    use rc_ledger::pass_state::{mark_terminal, PassKey, PassState};

    let dir = tempfile::tempdir().unwrap();
    let uid = "a".repeat(64);
    {
        let root = Root::open(dir.path()).unwrap();
        mark_terminal(
            root.ledger(),
            PassKey { turn_uid: &uid, pass_id: "event", family: "" },
            PassState::Done,
            None,
            1_700_000_000_000,
        )
        .unwrap();
        regress_to(root.ledger().conn(), "extract_pass_state", OLD_LEDGER_CHECK);
        assert!(!table_sql(root.ledger().conn(), "extract_pass_state").contains("'state'"));
            let after_regress: i64 = root
            .ledger()
            .conn()
            .query_row("SELECT count(*) FROM extract_pass_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(after_regress, 1, "夹具前提：退回旧 CHECK 后行必须还在");
    }
    let root = Root::open(dir.path()).unwrap();
    let conn = root.ledger().conn();
assert!(
        table_sql(conn, "extract_pass_state").contains("'state'"),
        "旧账库的 CHECK 没被加宽"
    );
    let kept: (String, String) = conn
        .query_row(
            "SELECT turn_uid, state FROM extract_pass_state WHERE pass_id='event'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(kept, (uid.clone(), "done".to_string()), "重建把旧行弄丢了");
    assert!(
        index_names(conn, "extract_pass_state").iter().any(|n| n.contains("extract_pass_state")),
        "索引没随表重建"
    );
    mark_terminal(
        root.ledger(),
        PassKey { turn_uid: &uid, pass_id: "state", family: "" },
        PassState::Done,
        None,
        1_700_000_000_001,
    )
    .unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM extract_pass_state WHERE pass_id='state'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    // 波三修 1：加宽到 `lesson` 这一格同样要写得进去（旧库第一次跑 lesson 撞 CHECK 的现场）。
    mark_terminal(
        root.ledger(),
        PassKey { turn_uid: &uid, pass_id: "lesson", family: "" },
        PassState::Empty,
        None,
        1_700_000_000_002,
    )
    .unwrap();
    let n_lesson: i64 = conn
        .query_row(
            "SELECT count(*) FROM extract_pass_state WHERE pass_id='lesson'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n_lesson, 1);
}
