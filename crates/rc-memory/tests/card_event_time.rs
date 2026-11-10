//! 债 #9（修法甲）：`card.time_json` 的读者侧解包 `reader::card_event_times`。
//!
//! 判据与写侧 `pass_time::card_time_json` 的 JSON 形状逐格对齐：
//! 只有 `kind='event_time'` 且 `start_ms` 为整数的卡进返回；`due`/`valid`/`recurrence`
//! 三族、坏 JSON、缺键、NULL 一律当"没有事件时间"跳过（读侧不许因为一行残值崩掉）。
//! 本文件夹具全部手写 INSERT（`card` 的 NOT NULL 列单见 `tests/ddl_shape.rs` 的同一形状），
//! 不走写侧 pass——写侧是禁改面，读者判据不依赖它的可达性。

use rc_memory::reader::card_event_times;
use rc_store::Root;

/// 一张最小卡（`card` 的 NOT NULL 列齐全），返回 id。
fn card(m: &rc_store::MemoryHandle, id: &str) {
    m.conn().execute(
        "INSERT INTO card(id,kind,title,text,scope_kind,scope_key,as_of_ms,
                          source_agent,device_id,created_at_ms,norm_version)
         VALUES(?1,'note','t','b','project','p',1,'agent-a','dev-1',1,1)",
        [id]).unwrap();
}

fn time_json(kind: &str, start: &str, end: &str) -> String {
    format!(r#"{{"kind":"{kind}","field":"occurred","start_ms":{start},"end_ms":{end},"precision":"minute","basis":"explicit"}}"#)
}

#[test]
fn event_time_kind_with_integer_start_is_returned() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    card(m, "cd-ev");
    m.conn().execute("UPDATE card SET time_json=?1 WHERE id='cd-ev'",
                     [time_json("event_time", "1700000000000", "null")]).unwrap();
    let got = card_event_times(m, &["cd-ev".to_string()]).unwrap();
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].id, "cd-ev");
    assert_eq!(got[0].start_ms, 1_700_000_000_000);
    assert_eq!(got[0].end_ms, None, "开口区间/单点事件的第二端是 None");
}

#[test]
fn range_event_carries_both_ends() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    card(m, "cd-range");
    m.conn().execute("UPDATE card SET time_json=?1 WHERE id='cd-range'",
                     [time_json("event_time", "1000", "2000")]).unwrap();
    let got = card_event_times(m, &["cd-range".to_string()]).unwrap();
    assert_eq!((got[0].start_ms, got[0].end_ms), (1000, Some(2000)), "{got:?}");
}

#[test]
fn non_event_kinds_and_bad_values_are_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    for id in ["cd-due", "cd-valid", "cd-rec", "cd-speech", "cd-no-start", "cd-badjson",
               "cd-null", "cd-strstart"] {
        card(m, id);
    }
    let set = |id: &str, json: &str| {
        m.conn().execute(&format!("UPDATE card SET time_json='{json}' WHERE id='{id}'"), []).unwrap();
    };
    set("cd-due", &time_json("deadline", "1000", "2000"));
    set("cd-valid", &time_json("validity", "1000", "2000"));
    set("cd-rec", &time_json("recurrence", "null", "null"));
    set("cd-speech", &time_json("speech_time", "1000", "null"));
    set("cd-no-start", r#"{"kind":"event_time","field":"occurred","end_ms":null}"#);
    set("cd-badjson", "{\"kind\":\"event_time\", 截断");
    set("cd-strstart", r#"{"kind":"event_time","start_ms":"1000"}"#);
    // `cd-null` 不写 ⇒ time_json IS NULL。
    let got = card_event_times(m, &[
        "cd-due".into(), "cd-valid".into(), "cd-rec".into(), "cd-speech".into(),
        "cd-no-start".into(), "cd-badjson".into(), "cd-null".into(), "cd-strstart".into(),
    ]).unwrap();
    assert!(got.is_empty(), "这八种残值一个都不许进事件读数：{got:?}");
}

#[test]
fn empty_input_short_circuits_and_duplicates_read_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    assert!(card_event_times(m, &[]).unwrap().is_empty(), "空入参不拼 IN ()");
    card(m, "cd-dup");
    m.conn().execute("UPDATE card SET time_json=?1 WHERE id='cd-dup'",
                     [time_json("event_time", "1000", "null")]).unwrap();
    let got = card_event_times(m, &["cd-dup".into(), "cd-dup".into()]).unwrap();
    assert_eq!(got.len(), 1, "重复 id 只读一次：{got:?}");
}

#[test]
fn unknown_ids_are_absent_not_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let got = card_event_times(m, &["cd-absent".to_string()]).unwrap();
    assert!(got.is_empty(), "查不到的 id = 没有事件时间，不是错误");
}
