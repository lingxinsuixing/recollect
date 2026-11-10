use rc_memory::schema::{Actor, ColumnOwner, MEMORY_TABLES, OWNERSHIP};
use rc_store::Root;

fn cols(conn: &rusqlite::Connection, table: &str) -> Vec<String> {
    let mut st = conn.prepare("SELECT name FROM pragma_table_info(?1) ORDER BY cid").unwrap();
    st.query_map([table], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok()).collect()
}

#[test]
fn every_registered_table_and_column_exists() {
    // 方向 ①：登记了但库里没有 ⇒ 归属表在说谎（将来 X 拿不到它以为能写的列）
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    // 表名也得管：下面那个循环由 MEMORY_TABLES 驱动、只按 o.table 过滤 OWNERSHIP，
    // 一条把表名拼错的登记在两个循环里都匹配不到任何表，于是"登记了不存在的表"能蒙混过关。
    // 这一趟由 OWNERSHIP 自己驱动：归属表多说出一张表清单外的表，当场红。
    for o in OWNERSHIP {
        assert!(MEMORY_TABLES.contains(&o.table), "{} 登记了归属却不在表清单里", o.table);
    }
    for t in MEMORY_TABLES {
        let have = cols(root.memory().conn(), t);
        assert!(!have.is_empty(), "归属表登记了 {t}，但库里没有这张表");
        for ColumnOwner { column, .. } in OWNERSHIP.iter().filter(|o| o.table == *t) {
            assert!(have.iter().any(|c| c == column), "{t}.{column} 登记了却不存在");
        }
    }
}

#[test]
fn every_existing_column_is_registered_default_deny() {
    // 方向 ②：库里有但没登记 ⇒ 默认拒绝。这是守卫 11"未登记列默认拒绝"唯一的执法点。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory().conn();
    let mut listed = Vec::new();
    {
        let mut st = m.prepare(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap().filter_map(|r| r.ok())
            .for_each(|t| listed.push(t));
    }
    // 索引层（rc_shadow / rc_index_marker）故意不在归属表里——守卫 2 要求影子表名在 rc-index 之外
    // 不可命名，把它登记进归属表等于承认它的列可读。过滤按"这一层索引的形状"，不是抄一份表名单：
    // 将来有人新增第三张虚表，它会出现在这条的 diff 里，而不是悄悄通过。
    // 形状是 1 + 5 + 1 而不是"两张"：rc_shadow 这张 fts5 虚表，加上它在 sqlite_master 里自带的
    // rc_shadow_config/_content/_data/_docsize/_idx 五个伴生条目，再加上 rc_index_marker 这一个 marker。
    // 所以按 rc_shadow_ 前缀滤——新前缀（第三张虚表）不匹配，照样红。
    let is_index_layer = |n: &str| n == "rc_shadow" || n.starts_with("rc_shadow_") || n == "rc_index_marker";
    let real: Vec<String> = listed.into_iter().filter(|t| !is_index_layer(t)).collect();
    assert_eq!(real, MEMORY_TABLES.to_vec(), "memory.db 的表清单与归属表不一致（清单现算，不抄名单）");
    for t in &real {
        let declared: Vec<String> = OWNERSHIP.iter().filter(|o| o.table == t.as_str())
            .map(|o| o.column.to_string()).collect();
        for c in cols(m, t) {
            assert!(declared.contains(&c), "{t}.{c} 未登记归属 ⇒ 默认拒绝");
        }
    }
}

#[test]
fn no_column_can_hold_a_score_or_an_importance() {
    // 守卫 7 + GOALS §3：库里不存检索分数列，也不存被引用次数回灌出来的重要性（D10）。
    // canonical DDL 里有意冻结了两族列名：抽取置信度（`*confidence`）与显著度读数
    // （`memory_stats.open_loop_weight` / `salience` / `emotional_intensity`）——它们是
    // schema §3/§4 的字段，不是检索排序分数，所以不在这条判据的 banned 里。
    // 剩下的词仍是"分数或重要性替身"的封闭清单：新增这类列照样红。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let banned = ["score", "bm25", "importance", "rank", "similarity",
                  "decay", "strength", "ref_count"];
    for t in MEMORY_TABLES {
        for c in cols(root.memory().conn(), t) {
            let lc = c.to_lowercase();
            assert!(!banned.iter().any(|b| lc.contains(b)), "{t}.{c} 是分数列或它的替身");
        }
    }
}

#[test]
fn ownership_registers_each_column_exactly_once() {
    // 派发档的硬要求：每一张表的每一列**恰好**一条登记。上面两条只对拍"存在性"，
    // 重复登记在两侧都能蒙混过去 ⇒ 这一条数三样：列数一致、无重复、集合一致。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for t in MEMORY_TABLES {
        let have = cols(root.memory().conn(), t);
        let mut declared: Vec<String> = OWNERSHIP.iter()
            .filter(|o| o.table == *t).map(|o| o.column.to_string()).collect();
        declared.sort();
        let mut uniq = declared.clone();
        uniq.dedup();
        assert_eq!(declared, uniq, "{t} 有列被重复登记");
        assert_eq!(declared.len(), have.len(), "{t} 的登记列数与库里列数不一致");
    }
}

#[test]
fn every_actor_has_at_least_one_column() {
    // 反向对拍：OWNERSHIP 里某个 Actor 一列都没有 ⇒ 那个写入者类型是装饰（任务 3 会写出空壳）
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let _ = root.memory().conn();
    for a in [Actor::Program, Actor::Extractor, Actor::Center, Actor::Agent] {
        assert!(OWNERSHIP.iter().any(|o| o.actor == a), "{a:?} 一列都没有");
    }
}
