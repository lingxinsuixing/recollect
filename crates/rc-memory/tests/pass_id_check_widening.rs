//! P2/A9：CHECK 闭集加宽时**旧库**必须被 `rc-store` 的 `widen_closed_set_check`
//! 重建跟上。
//!
//! 为什么这条用例必须有：fresh 库由 `apply` 直接按 canonical 建表，永远测不到迁移那一支；
//! 而真实存在的库（本机实验目录、中心数据目录、测试机）都是旧形状。少了这一条，
//! "旧库第一次跑 `distill --pass state` / 第一条 `recollect_report` 会不会撞 CHECK"
//! 只能靠上线时踩。
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
        // 波三修 1：canonical 的 pass_id CHECK 再含 `'lesson'`（第 6 pass 的终态行）。
        "extract_pass_state" =>
            "CHECK(pass_id IN ('naming','fact','event','time','state','lesson'))",
        // A9：canonical 的 pass_id CHECK 已含 `'report'`（与 source CHECK 同一次加宽）。
        // 波二刀 B：再含 `'reuse'`（复用反馈的 failed/mismatch 入复核队列）。
        // 波三刀 A：再含 `'lesson'`（第 6 pass 的审核项走同一张队列）。
        _ => "CHECK(pass_id IN ('naming','fact','event','time','rows','state','report','reuse','lesson'))",
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

const OLD_REVIEW_CHECK: &str = "CHECK(pass_id IN ('naming','fact','event','time','rows'))";

fn item(pass_id: &str, uid: &str, payload: &str) -> rc_memory::review::NewItem {
    rc_memory::review::NewItem {
        source: "distill".into(),
        pass_id: pass_id.into(),
        family: String::new(),
        session_key: "sess-old".into(),
        device_id: "dev-old".into(),
        turn_uids_json: format!("[\"{uid}\"]"),
        owner_json: "{\"owner_agent_id\":\"alice\"}".into(),
        payload_json: payload.into(),
        batch_key: String::new(),
    }
}

#[test]
fn an_old_memory_db_gets_review_item_widened_and_keeps_its_unique_index() {
    let dir = tempfile::tempdir().unwrap();
    let uid = "b".repeat(64);
    {
        let root = Root::open(dir.path()).unwrap();
        rc_memory::review::enqueue(
            root.memory(),
            &item("event", &uid, "{\"schema\":\"review.pass.v1\",\"a\":1}"),
            1_700_000_000_000,
        )
        .unwrap();
        regress_to(root.memory().conn(), "review_item", OLD_REVIEW_CHECK);
        assert!(!table_sql(root.memory().conn(), "review_item").contains("'state'"));
    }
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    assert!(
        table_sql(conn, "review_item").contains("'state'"),
        "旧记忆库的 review_item 没被加宽"
    );
    let kept: i64 = conn
        .query_row("SELECT count(*) FROM review_item WHERE pass_id='event'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(kept, 1, "重建把旧 item 弄丢了");
    let indexes = index_names(conn, "review_item");
    assert!(
        indexes.iter().any(|name| name == "review_item_payload"),
        "载荷唯一索引没重建：{indexes:?}"
    );
    rc_memory::review::enqueue(
        root.memory(),
        &item("state", &uid, "{\"schema\":\"review.pass.v1\",\"b\":2}"),
        1_700_000_000_001,
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM review_item WHERE pass_id='state'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

/// A9（W34 2026-11-07）：`source` 的 CHECK 加宽到含 `'report'` 时，**旧库**（只认
/// `'distill'/'rows'`）必须被同一次重建跟上——重建走的是 canonical 全文，所以一条
/// 探针（`'report'`）同时把 source 与 pass_id 两个 CHECK 拓到新形状。
#[test]
fn an_old_memory_db_gets_the_report_source_check_widened() {
    const NEW_SOURCE_CHECK: &str = "CHECK(source IN ('distill','rows','report','reuse'))";
    const OLD_SOURCE_CHECK: &str = "CHECK(source IN ('distill','rows'))";
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        let conn = root.memory().conn();
        // 把 source 的 CHECK 退回旧形状（pass_id 不动——生产旧库正是"有 state、没 report"）。
        let current = table_sql(conn, "review_item");
        assert!(current.contains(NEW_SOURCE_CHECK), "夹具前提破了：canonical 里没有新 CHECK");
        let old = current.replace(NEW_SOURCE_CHECK, OLD_SOURCE_CHECK);
        let staged = "review_item__old";
        conn.execute_batch(&old.replace("CREATE TABLE review_item(", "CREATE TABLE review_item__old(")).unwrap();
        let columns: Vec<String> = {
            let mut st = conn
                .prepare("SELECT name FROM pragma_table_info('review_item') ORDER BY cid")
                .unwrap();
            st.query_map([], |r| r.get::<_, String>(0)).unwrap()
                .collect::<Result<Vec<_>, _>>().unwrap()
        };
        let list = columns.join(",");
        conn.execute_batch(&format!(
            "INSERT INTO {staged}({list}) SELECT {list} FROM review_item;
             DROP TABLE review_item;
             ALTER TABLE {staged} RENAME TO review_item;"
        ))
        .unwrap();
        // 退回成功 = 新 source CHECK 不在场、旧 source CHECK 在场（pass_id 侧的
        // `'report'` 字面量还在，所以探针不能按裸 `'report'` 判，要按整条 CHECK 判）。
        let regressed = table_sql(conn, "review_item");
        assert!(!regressed.contains(NEW_SOURCE_CHECK) && regressed.contains(OLD_SOURCE_CHECK),
            "夹具退回失败：{regressed}");
    }
    // 重开库：source 探针（canonical 全文）缺失 ⇒ 重建 ⇒ source 列写 `'report'` 通行，
    // pass_id 探针同时被 canonical 拓到新形状。旧行保留。
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    assert!(
        table_sql(conn, "review_item").contains(NEW_SOURCE_CHECK),
        "旧记忆库的 source CHECK 没被加宽"
    );
    // 上面的 `item()` 助手把 source 写死成 "distill"；报告项要的正是 `'report'`
    // 这一格，所以这里显式构造。
    let report_item = rc_memory::review::NewItem {
        source: "report".into(),
        ..item(
            "report",
            &"c".repeat(64),
            "{\"schema\":\"report.v1\",\"category\":\"wrong_fact\"}",
        )
    };
    rc_memory::review::enqueue(root.memory(), &report_item, 1_700_000_000_002).unwrap();
    let (src, n): (String, i64) = conn
        .query_row(
            "SELECT source, count(*) FROM review_item WHERE pass_id='report'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((src.as_str(), n), ("report", 1), "报告项没落进旧库加宽后的表");
}

/// 波二刀 B：`source='reuse'` / `pass_id='reuse'` 在 **A9 形状的旧库**上也要写得进去。
///
/// 与上一条同一条路（探针缺失 ⇒ canonical 全文重建），只是旧形状停在 A9
/// （`source` 到 `'report'`、`pass_id` 到 `'report'`），新探针里多出来的 `'reuse'` 是
/// 触发重建的那个字。少了这条用例，"现网旧库第一次跑 `hub review reuse-ingest` 会不会
/// 撞 CHECK"只能等上线时踩。
#[test]
fn an_old_memory_db_accepts_reuse_items_after_widening() {
    const A9_SOURCE_CHECK: &str = "CHECK(source IN ('distill','rows','report'))";
    const NEW_SOURCE_CHECK: &str = "CHECK(source IN ('distill','rows','report','reuse'))";
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        let conn = root.memory().conn();
        // 退回 A9 的两条 CHECK（source 与 pass_id 各换回不含 `'reuse'` 的那一条）。
        let current = table_sql(conn, "review_item");
        assert!(current.contains(NEW_SOURCE_CHECK), "夹具前提破了：canonical 里没有新 CHECK");
        let a9 = current
            .replace(
                NEW_SOURCE_CHECK,
                A9_SOURCE_CHECK,
            )
            .replace(
                "CHECK(pass_id IN ('naming','fact','event','time','rows','state','report','reuse','lesson'))",
                "CHECK(pass_id IN ('naming','fact','event','time','rows','state','report'))",
            );
        assert!(a9 != current, "夹具退回没换掉任何一条 CHECK");
        conn.execute_batch(&a9.replace(
            "CREATE TABLE review_item(",
            "CREATE TABLE review_item__old(",
        ))
        .unwrap();
        let columns: Vec<String> = {
            let mut st = conn
                .prepare("SELECT name FROM pragma_table_info('review_item') ORDER BY cid")
                .unwrap();
            st.query_map([], |r| r.get::<_, String>(0)).unwrap()
                .collect::<Result<Vec<_>, _>>().unwrap()
        };
        let list = columns.join(",");
        conn.execute_batch(&format!(
            "INSERT INTO review_item__old({list}) SELECT {list} FROM review_item;
             DROP TABLE review_item;
             ALTER TABLE review_item__old RENAME TO review_item;"
        ))
        .unwrap();
        assert!(!table_sql(conn, "review_item").contains("'reuse'"), "夹具退回失败");
    }
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    assert!(
        table_sql(conn, "review_item").contains(NEW_SOURCE_CHECK),
        "旧记忆库的 source CHECK 没被加宽到 reuse"
    );
    let reuse_item = rc_memory::review::NewItem {
        source: "reuse".into(),
        ..item(
            "reuse",
            &"d".repeat(64),
            "{\"schema\":\"reuse.v1\",\"card\":\"card:x\",\"outcome\":\"failed\"}",
        )
    };
    rc_memory::review::enqueue(root.memory(), &reuse_item, 1_700_000_000_003).unwrap();
    let (src, n): (String, i64) = conn
        .query_row(
            "SELECT source, count(*) FROM review_item WHERE pass_id='reuse'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((src.as_str(), n), ("reuse", 1), "复用待审项没落进旧库加宽后的表");
}

/// 波三刀 A：`pass_id='lesson'` 在 **波二形状的旧库**（`pass_id` 到 `'reuse'`）上也要写得进去。
///
/// 与 `reuse` 那条同一条路（探针缺失 ⇒ canonical 全文重建），只是旧形状停在波二
/// （`source` 已到 `'reuse'`、`pass_id` 到 `'reuse'`）。少了这条用例，"现网旧库第一次跑
/// `hub distill --pass lesson` 会不会撞 CHECK"只能等上线时踩。
#[test]
fn an_old_memory_db_accepts_lesson_items_after_widening() {
    const W2_PASS_CHECK: &str =
        "CHECK(pass_id IN ('naming','fact','event','time','rows','state','report','reuse'))";
    const NEW_PASS_CHECK: &str = "CHECK(pass_id IN ('naming','fact','event','time','rows','state','report','reuse','lesson'))";
    let dir = tempfile::tempdir().unwrap();
    {
        let root = Root::open(dir.path()).unwrap();
        let conn = root.memory().conn();
        let current = table_sql(conn, "review_item");
        assert!(current.contains(NEW_PASS_CHECK), "夹具前提破了：canonical 里没有新 CHECK");
        let old = current.replace(NEW_PASS_CHECK, W2_PASS_CHECK);
        assert!(old != current, "夹具退回没换掉任何一条 CHECK");
        conn.execute_batch(&old.replace(
            "CREATE TABLE review_item(",
            "CREATE TABLE review_item__old(",
        ))
        .unwrap();
        let columns: Vec<String> = {
            let mut st = conn
                .prepare("SELECT name FROM pragma_table_info('review_item') ORDER BY cid")
                .unwrap();
            st.query_map([], |r| r.get::<_, String>(0)).unwrap()
                .collect::<Result<Vec<_>, _>>().unwrap()
        };
        let list = columns.join(",");
        conn.execute_batch(&format!(
            "INSERT INTO review_item__old({list}) SELECT {list} FROM review_item;
             DROP TABLE review_item;
             ALTER TABLE review_item__old RENAME TO review_item;"
        ))
        .unwrap();
        assert!(!table_sql(conn, "review_item").contains("'lesson'"), "夹具退回失败");
    }
    let root = Root::open(dir.path()).unwrap();
    let conn = root.memory().conn();
    assert!(
        table_sql(conn, "review_item").contains(NEW_PASS_CHECK),
        "旧记忆库的 pass_id CHECK 没被加宽到 lesson"
    );
    let lesson_item = item(
        "lesson",
        &"e".repeat(64),
        "{\"schema\":\"review.pass.v1\",\"pass_id\":\"lesson\"}",
    );
    rc_memory::review::enqueue(root.memory(), &lesson_item, 1_700_000_000_004).unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM review_item WHERE pass_id='lesson'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "lesson 待审项没落进旧库加宽后的表");
}
