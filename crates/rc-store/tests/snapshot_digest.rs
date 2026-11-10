use rc_store::snapshot::logical_digest;
use rusqlite::Connection;

const DDL: &str = "CREATE TABLE t(a TEXT, b TEXT, PRIMARY KEY(a, b));";

fn db(dir: &std::path::Path, name: &str) -> Connection {
    Connection::open(dir.join(name)).unwrap()
}

#[test]
fn digest_is_order_independent_and_content_sensitive() {
    let dir = tempfile::tempdir().unwrap();
    let a = db(dir.path(), "a.db");
    let b = db(dir.path(), "b.db");
    a.execute_batch(DDL).unwrap();
    b.execute_batch(DDL).unwrap();
    a.execute_batch("INSERT INTO t VALUES('x','1'),('y','2');").unwrap();
    b.execute_batch("INSERT INTO t VALUES('y','2'),('x','1');").unwrap();
    let da = logical_digest(&a, &["t"]).unwrap();
    assert_eq!(da, logical_digest(&b, &["t"]).unwrap(), "插入顺序不该影响摘要");

    b.execute("UPDATE t SET b='3' WHERE a='y'", []).unwrap();
    assert_ne!(da, logical_digest(&b, &["t"]).unwrap(), "内容变了摘要必须变");

    // VACUUM 改的是文件布局，不是内容。
    a.execute_batch("VACUUM;").unwrap();
    assert_eq!(da, logical_digest(&a, &["t"]).unwrap(), "VACUUM 不该改摘要");
}

#[test]
fn null_and_empty_text_are_different() {
    let dir = tempfile::tempdir().unwrap();
    let a = db(dir.path(), "a.db");
    let b = db(dir.path(), "b.db");
    a.execute_batch("CREATE TABLE t(id TEXT PRIMARY KEY, body TEXT);").unwrap();
    b.execute_batch("CREATE TABLE t(id TEXT PRIMARY KEY, body TEXT);").unwrap();
    a.execute("INSERT INTO t VALUES('x',NULL)", []).unwrap();
    b.execute("INSERT INTO t VALUES('x','')", []).unwrap();
    assert_ne!(
        logical_digest(&a, &["t"]).unwrap(),
        logical_digest(&b, &["t"]).unwrap(),
        "NULL 与空串撞车 ⇒ 类型标签没起作用"
    );
}

#[test]
fn text_boundaries_do_not_collide() {
    let dir = tempfile::tempdir().unwrap();
    let a = db(dir.path(), "a.db");
    let b = db(dir.path(), "b.db");
    a.execute_batch(DDL).unwrap();
    b.execute_batch(DDL).unwrap();
    // 朴素拼接会把 ('a','bc') 与 ('ab','c') 编码成同一个 "abc"。
    a.execute("INSERT INTO t VALUES('a','bc')", []).unwrap();
    b.execute("INSERT INTO t VALUES('ab','c')", []).unwrap();
    assert_ne!(
        logical_digest(&a, &["t"]).unwrap(),
        logical_digest(&b, &["t"]).unwrap(),
        "两个字段的边界没被编码进去"
    );
}

#[test]
fn unknown_or_keyless_table_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let c = db(dir.path(), "c.db");
    c.execute_batch("CREATE TABLE keyless(v TEXT);").unwrap();
    assert!(logical_digest(&c, &["absent"]).is_err(), "不存在的表不许静默跳过");
    assert!(logical_digest(&c, &["keyless"]).is_err(), "没有主键的表没有稳定行序");
}

#[test]
fn table_order_does_not_matter_but_the_table_set_does() {
    let dir = tempfile::tempdir().unwrap();
    let c = db(dir.path(), "c.db");
    c.execute_batch(
        "CREATE TABLE t(id TEXT PRIMARY KEY);
         CREATE TABLE u(id TEXT PRIMARY KEY);
         INSERT INTO t VALUES('1');
         INSERT INTO u VALUES('2');",
    )
    .unwrap();
    assert_eq!(
        logical_digest(&c, &["t", "u"]).unwrap(),
        logical_digest(&c, &["u", "t"]).unwrap(),
        "输入顺序不该影响摘要"
    );
    assert_ne!(
        logical_digest(&c, &["t"]).unwrap(),
        logical_digest(&c, &["t", "u"]).unwrap(),
        "少一张表必须换一个摘要，否则新增表会被静默漏掉"
    );
}
