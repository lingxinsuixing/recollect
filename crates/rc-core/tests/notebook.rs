//! 小本本（W36）`core_note_<h>` 的库层测试钉（细案 §7-1）：
//! 触发器拒 UPDATE/DELETE、同毫秒同文本不撞 id、list 回执零正文字段、expand 全文逐字节、
//! 未注册 ⇒ `Unregistered`、revoke 后表消失、开库给存量 agent 幂等补建、形状校验整条不落、
//! 以及 `brief` 的索引栏接线。

use rc_core::crypto::note_table_name;
use rc_core::ops::{NotebookAppend, NotebookEntry, NOTEBOOK_BODY_MAX, NOTEBOOK_TAG_MAX, NOTEBOOK_TAGS_MAX};
use rc_core::{CoreError, CoreStore};
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;

const NOW: i64 = 1_700_000_000_000;

fn new_store() -> (tempfile::TempDir, CoreStore) {
    let dir = tempfile::tempdir().unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    (dir, store)
}

fn db(dir: &Path) -> Connection {
    Connection::open(dir.join("core.db")).unwrap()
}

fn table_names_in(dir: &Path) -> Vec<String> {
    let conn = db(dir);
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn append(
    store: &CoreStore,
    agent_id: &str,
    title: Option<&str>,
    tags: &[&str],
    body: &str,
    now_ms: i64,
) -> NotebookAppend {
    let tags: Vec<String> = tags.iter().map(|t| t.to_string()).collect();
    store
        .notebook_append(agent_id, title, &tags, body, None, "dev-1", now_ms)
        .unwrap()
}

#[test]
fn register_creates_the_note_table_and_triggers_refuse_update_and_delete() {
    let (dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    let note = note_table_name("agent-a");
    assert!(table_names_in(dir.path()).contains(&note), "{:?}", table_names_in(dir.path()));

    append(&store, "agent-a", Some("标题"), &["tag"], "第一条", NOW);
    let conn = db(dir.path());
    // append-only 的物理表达是两个触发器，不是约定：UPDATE/DELETE 必须被 ABORT，文本照 DDL。
    let upd = conn.execute(&format!("UPDATE \"{note}\" SET created_at_ms=1"), []);
    let err = upd.unwrap_err();
    assert!(
        err.to_string().contains("notebook is append-only"),
        "UPDATE 的错误没带 ABORT 文本：{err}"
    );
    let del = conn.execute(&format!("DELETE FROM \"{note}\""), []);
    let err = del.unwrap_err();
    assert!(
        err.to_string().contains("notebook is append-only"),
        "DELETE 的错误没带 ABORT 文本：{err}"
    );
    // 两次被拒的写都没改成任何东西。
    let (rows, at): (i64, i64) = conn
        .query_row(&format!("SELECT COUNT(*), MAX(created_at_ms) FROM \"{note}\""), [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((rows, at), (1, NOW));
}

#[test]
fn same_ms_same_text_second_row_does_not_collide() {
    let (_dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    let first = append(&store, "agent-a", None, &[], "同毫秒同文本", NOW);
    let second = append(&store, "agent-a", None, &[], "同毫秒同文本", NOW);
    assert_ne!(first.id, second.id, "index 参与摘要 ⇒ 同毫秒同文本不撞 id");
    assert_eq!(first.created_at_ms, second.created_at_ms);

    // 同 id 落不进第二行（主键闸仍然在）。
    let list = store.notebook_list("agent-a", None, None).unwrap();
    assert_eq!(list.notes.len(), 2);
}

#[test]
fn list_receipt_has_no_body_key() {
    let (_dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    append(
        &store,
        "agent-a",
        Some("给下个会话的便签"),
        &["工作"],
        "这里有一段不该被 list 泄出去的正文",
        NOW,
    );
    let list = store.notebook_list("agent-a", None, None).unwrap();
    let v = serde_json::to_value(&list).unwrap();
    let serialized = serde_json::to_string(&v).unwrap();
    assert!(
        !serialized.contains("body"),
        "list 回执带出了 body 键：{serialized}"
    );
    assert_eq!(list.notes.len(), 1);
    assert_eq!(list.unlisted, 0);
    let Value::Array(rows) = &v["notes"] else {
        panic!("notes 不是数组：{v:?}")
    };
    let Value::Object(row) = &rows[0] else { panic!("行不是对象：{v:?}") };
    let keys: Vec<&String> = row.keys().collect();
    assert_eq!(
        keys.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        vec!["created_at_ms", "id", "tags", "title"],
        "list 回执的键集漂了：{keys:?}"
    );
}

#[test]
fn expand_returns_the_body_byte_for_byte() {
    let (_dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    // 多字节、换行、空白都原样回——expand 是唯一回全文的口。
    let body = "第一行\n第二行：中文、emoji 占位 𝕏、tab\t与尾随空格   ";
    let tags = vec!["长文".to_string()];
    let note = store
        .notebook_append("agent-a", Some("标题"), &tags, body, Some("t-42"), "dev-1", NOW)
        .unwrap();
    let full = store.notebook_expand("agent-a", &note.id).unwrap();
    assert_eq!(full.body, body, "expand 不是逐字节回原文");
    assert_eq!(full.title, "标题");
    assert_eq!(full.tags, vec!["长文".to_string()]);
    assert_eq!(full.source_turn_uid.as_deref(), Some("t-42"));
    assert_eq!(full.created_at_ms, NOW);

    // 指不到的句柄：NotFound（句柄形状没问题只是没指到东西）。
    assert!(matches!(
        store.notebook_expand("agent-a", "n-000000000000"),
        Err(CoreError::NotFound)
    ));
}

#[test]
fn unregistered_agents_are_refused_on_all_four_operations() {
    let (_dir, store) = new_store();
    let tags: Vec<String> = vec!["tag".to_string()];
    assert!(matches!(
        store.notebook_append("agent-a", None, &tags, "正文", None, "dev", NOW),
        Err(CoreError::Unregistered)
    ));
    assert!(matches!(
        store.notebook_list("agent-a", None, None),
        Err(CoreError::Unregistered)
    ));
    assert!(matches!(
        store.notebook_search("agent-a", "词", None),
        Err(CoreError::Unregistered)
    ));
    assert!(matches!(
        store.notebook_expand("agent-a", "n-000000000000"),
        Err(CoreError::Unregistered)
    ));
}

#[test]
fn revoke_drops_the_note_table() {
    let (dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    store.register_agent("agent-b", 0).unwrap();
    append(&store, "agent-a", None, &[], "会被一起撤掉", NOW);
    store.revoke_agent("agent-a").unwrap();
    let names = table_names_in(dir.path());
    assert!(!names.contains(&note_table_name("agent-a")), "{names:?}");
    assert!(names.contains(&note_table_name("agent-b")), "{names:?}");
    assert!(matches!(
        store.notebook_list("agent-a", None, None),
        Err(CoreError::Unregistered)
    ));
}

#[test]
fn reopening_the_store_backfills_missing_note_tables_for_existing_agents() {
    let (dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    let note = note_table_name("agent-a");
    // 模拟"表是 W36 之后才有的"存量库：手工把表拆掉再重开。
    db(dir.path())
        .execute_batch(&format!("DROP TABLE \"{note}\""))
        .unwrap();
    assert!(!table_names_in(dir.path()).contains(&note));

    let store = CoreStore::open(dir.path()).unwrap();
    assert!(
        table_names_in(dir.path()).contains(&note),
        "开库没给存量 agent 补建小本本表"
    );
    // 补建的表照常可用，且注册行没被重写（幂等）。
    append(&store, "agent-a", None, &[], "补建后第一条", NOW);
    assert_eq!(store.list_agents().unwrap().len(), 1);
}

#[test]
fn shape_violations_reject_the_whole_entry() {
    let (_dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    let tags: Vec<String> = Vec::new();
    // body 空。
    assert!(matches!(
        store.notebook_append("agent-a", None, &tags, "   ", None, "dev", NOW),
        Err(CoreError::BadPayload(e)) if e.contains("empty")
    ));
    // body 超 2000 **字符**（按字符不按字节：2000 个汉字合法，2001 个非法）。
    let ok = "汉".repeat(NOTEBOOK_BODY_MAX);
    append(&store, "agent-a", None, &[], &ok, NOW);
    let too_long = "汉".repeat(NOTEBOOK_BODY_MAX + 1);
    assert!(matches!(
        store.notebook_append("agent-a", None, &tags, &too_long, None, "dev", NOW),
        Err(CoreError::BadPayload(e)) if e.contains("2000")
    ));
    // tags 超 8 条。
    let too_many: Vec<String> = (0..NOTEBOOK_TAGS_MAX + 1).map(|i| format!("t{i}")).collect();
    assert!(matches!(
        store.notebook_append("agent-a", None, &too_many, "正文", None, "dev", NOW),
        Err(CoreError::BadPayload(e)) if e.contains("tags")
    ));
    // 单条 tag 超 32 字符（33 个汉字）。
    let long_tag: Vec<String> = vec!["标".repeat(NOTEBOOK_TAG_MAX + 1)];
    assert!(matches!(
        store.notebook_append("agent-a", None, &long_tag, "正文", None, "dev", NOW),
        Err(CoreError::BadPayload(e)) if e.contains("32")
    ));
    // 空 tag 也是形状错。
    let empty_tag: Vec<String> = vec!["  ".to_string()];
    assert!(matches!(
        store.notebook_append("agent-a", None, &empty_tag, "正文", None, "dev", NOW),
        Err(CoreError::BadPayload(e)) if e.contains("tag is empty")
    ));
    // 五次违例一条都没落：库里只有最早那条合法行。
    let list = store.notebook_list("agent-a", None, None).unwrap();
    assert_eq!(list.notes.len(), 1);
}

#[test]
fn list_orders_newest_first_caps_limit_and_filters_by_tag() {
    let (_dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    for i in 0..5 {
        let tags: Vec<&str> = if i % 2 == 0 { vec!["偶"] } else { vec![] };
        append(&store, "agent-a", Some(&format!("第 {i} 条")), &tags, &format!("正文 {i}"), NOW + i);
    }
    let list = store.notebook_list("agent-a", None, None).unwrap();
    let stamps: Vec<i64> = list.notes.iter().map(|n| n.created_at_ms).collect();
    let mut sorted = stamps.clone();
    sorted.sort_unstable();
    sorted.reverse();
    assert_eq!(stamps, sorted, "list 不是新的在前");

    let tagged = store.notebook_list("agent-a", None, Some("偶")).unwrap();
    assert_eq!(tagged.notes.len(), 3);
    assert!(tagged.notes.iter().all(|n| n.tags == vec!["偶".to_string()]));

    let small = store.notebook_list("agent-a", Some(2), None).unwrap();
    assert_eq!(small.notes.len(), 2);
    assert_eq!(small.unlisted, 3, "unlisted = 过滤后在场 − 已列出");

    // limit 封顶 200；0 视为缺省 50。
    assert_eq!(store.notebook_list("agent-a", Some(999), None).unwrap().notes.len(), 5);
    assert_eq!(store.notebook_list("agent-a", Some(0), None).unwrap().notes.len(), 5);
}

#[test]
fn search_is_substring_match_over_decrypted_rows_with_or_semantics() {
    let (_dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    append(&store, "agent-a", Some("露营清单"), &["周末"], "带上炉头和气罐", NOW);
    append(&store, "agent-a", Some("搬家备忘"), &[], "下周约货车", NOW + 1);
    append(&store, "agent-a", None, &["周末"], "货车租赁比价", NOW + 2);

    // 单词命中 body。
    let hits = store.notebook_search("agent-a", "炉头", None).unwrap();
    assert_eq!(hits.notes.len(), 1);
    assert_eq!(hits.notes[0].title, "露营清单");
    // 多词是「或」：任一词命中即算（"货车"与"炉头"各中一条，第三条命中两次只算一条）。
    let hits = store.notebook_search("agent-a", "货车 炉头", None).unwrap();
    assert_eq!(hits.notes.len(), 3);
    // 标题与标签也在匹配面上。
    assert_eq!(store.notebook_search("agent-a", "搬家", None).unwrap().notes.len(), 1);
    assert_eq!(store.notebook_search("agent-a", "周末", None).unwrap().notes.len(), 2);
    // 空命中是合法空读数，不是错误。
    let none = store.notebook_search("agent-a", "不存在的词", None).unwrap();
    assert!(none.notes.is_empty());
    assert_eq!(none.unlisted, 0);
    // 空 query 是形状错。
    assert!(matches!(
        store.notebook_search("agent-a", "   ", None),
        Err(CoreError::BadPayload(e)) if e.contains("query is empty")
    ));
}

#[test]
fn brief_shows_the_notebook_section_after_an_append() {
    let (_dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    // 空载荷 + 无小本本 ⇒ 只有头部（沉默匹配视图内容的既有钉，这里随 W36 复核一遍）。
    let empty = store
        .brief("agent-a", "user-1", "daily", 20_000, None, NOW)
        .unwrap();
    assert!(!empty.text.contains("[小本本]"), "{}", empty.text);

    // 只写了小本本的新 agent：payload 仍是"空"，但简报必须出小本本段。
    append(&store, "agent-a", Some("炉头清单"), &["露营"], "带上炉头和气罐", NOW - 3_600_000);
    let brief = store
        .brief("agent-a", "user-1", "daily", 20_000, None, NOW)
        .unwrap();
    assert!(brief.text.contains("[小本本]"), "{}", brief.text);
    assert!(brief.text.contains("炉头清单"), "{}", brief.text);
    assert!(brief.text.contains("露营"), "{}", brief.text);
    assert!(brief.text.contains("1 小时前"), "{}", brief.text);
    assert!(brief.section_tokens.contains_key("notebook"), "{:?}", brief.section_tokens);
    // 注入日志照写（读数走 core_injection_log，区名进 section_tokens_json）。
    let logged: String = db_path(&_dir)
        .query_row(
            "SELECT section_tokens_json FROM core_injection_log ORDER BY id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(logged.contains("notebook"), "{logged}");
}

fn db_path(dir: &tempfile::TempDir) -> Connection {
    Connection::open(dir.path().join("core.db")).unwrap()
}

/// 索引行的形状钉（brief 索引栏的输入）：`NotebookEntry` 没有 body/source_turn_uid。
#[test]
fn notebook_entry_projection_has_no_body_field() {
    let entry = NotebookEntry {
        id: "n-0123456789ab".to_string(),
        title: "标题".to_string(),
        tags: vec!["a".to_string()],
        created_at_ms: NOW,
    };
    let serialized = serde_json::to_string(&entry).unwrap();
    assert!(!serialized.contains("body"), "{serialized}");
    assert!(!serialized.contains("source_turn_uid"), "{serialized}");
}
