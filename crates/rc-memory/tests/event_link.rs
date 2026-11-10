//! A44 的三条读数：软边在场（两端 + 依据）、链长上限 2（只挡边不挡行）、
//! 撤销是账不是删行。夹具一律走生产写入口。

use rc_memory::keys::{Scope, ScopeKind};
use rc_memory::writers_c::retire_event_link;
use rc_memory::writers_x::{put_card, put_event, put_timeline_rows, put_timeline_rows_counted,
                           FactContent, TimelineContent};
use rc_memory::{reader, BatchCtx, Provenance};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "p-event-link".into() } }
fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: rc_memory::keys::AnchorRole::Human }]
}

fn timeline(body: &str, anchor: &str, basis: Vec<String>) -> TimelineContent {
    TimelineContent {
        title: format!("derive-{body}"),
        body: body.into(),
        scope: scope(),
        time_anchor_ms: Some(NOW + 1),
        time_anchor_id: Some(anchor.into()),
        time_confidence: "anchored".into(),
        basis_ids: basis,
    }
}

#[test]
fn 时间行锚在事件上时落一条软边_两端与依据都在位() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let meeting = put_event(m, &FactContent {
        kind: "meeting".into(), title: "季度会".into(),
        body: "会上定了十月发布。".into(), scope: scope(), valid_to_ms: None,
    }, &prov('a'), NOW).unwrap();
    let basis = put_card(m, &FactContent {
        kind: "decision".into(), title: "发布决定".into(),
        body: "十月发布。".into(), scope: scope(), valid_to_ms: None,
    }, &prov('b'), NOW).unwrap();

    let n = put_timeline_rows(m, &[timeline("发布排在季度会之后。", &meeting.id, vec![basis.id.clone()])],
                              &BatchCtx::synthetic(), NOW).unwrap();
    assert_eq!(n, 1);
    let tl: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' AND text='发布排在季度会之后。'",
        [], |r| r.get(0)).unwrap();

    let got: (String, String, String, String, Option<String>) = m.conn().query_row(
        "SELECT from_id, to_id, basis, evidence_json, decided_by FROM fact_link", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).unwrap();
    assert_eq!(got.0, format!("card:{}", meeting.id), "from 端 = 被锚事件（\"<table>:<id>\" 键）");
    assert_eq!(got.1, format!("card:{tl}"), "to 端 = 新落的推导时间行");
    assert_eq!(got.2, "发布排在季度会之后。", "basis = 时间行正文（凭什么连）");
    assert_eq!(got.3, format!("[\"{}\"]", basis.id), "evidence_json = 时间行 basis_ids 的 JSON 数组");
    assert_eq!(got.4, None, "decided_by 归 C，今天无生产者");

    let l = reader::event_link_of(m, &tl).unwrap().expect("读侧必须看得见这条软边");
    assert_eq!((l.a_id.as_str(), l.b_id.as_str()),
               (format!("card:{}", meeting.id).as_str(), format!("card:{tl}").as_str()));
    assert_eq!(l.scope.key, "p-event-link");
}

#[test]
fn 软边链长上限二_第三跳只挡边不挡行() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let meeting = put_event(m, &FactContent {
        kind: "meeting".into(), title: "根事件".into(), body: "根。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('a'), NOW).unwrap();

    put_timeline_rows(m, &[timeline("第一跳", &meeting.id, vec![])], &BatchCtx::synthetic(), NOW).unwrap();
    let t1: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' AND text='第一跳'", [], |r| r.get(0)).unwrap();
    put_timeline_rows(m, &[timeline("第二跳", &t1, vec![])], &BatchCtx::synthetic(), NOW).unwrap();
    let t2: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' AND text='第二跳'", [], |r| r.get(0)).unwrap();
    put_timeline_rows(m, &[timeline("第三跳", &t2, vec![])], &BatchCtx::synthetic(), NOW).unwrap();

    let links: i64 = m.conn().query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap();
    assert_eq!(links, 2, "深度 0 与 1 各落一条；深度 2 的第三跳必须被上限挡住");
    let rows: i64 = m.conn().query_row(
        "SELECT count(*) FROM card WHERE kind='timeline' AND text='第三跳'", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "上限只挡软边：时间行与它的锚列照落（不是数据丢弃）");
}

#[test]
fn 撤销软边_读侧立刻看不见但账与行都还在() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let meeting = put_event(m, &FactContent {
        kind: "meeting".into(), title: "要被撤销的会".into(), body: "会。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('a'), NOW).unwrap();
    put_timeline_rows(m, &[timeline("撤销夹具", &meeting.id, vec![])], &BatchCtx::synthetic(), NOW).unwrap();
    let tl: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='timeline' AND text='撤销夹具'", [], |r| r.get(0)).unwrap();
    let link: String = m.conn().query_row("SELECT id FROM fact_link", [], |r| r.get(0)).unwrap();

    retire_event_link(m, &link, "判同撤销", NOW + 5).unwrap();
    assert!(reader::event_link_of(m, &tl).unwrap().is_none(), "撤销后读侧必须看不见这条软边");
    let rows: i64 = m.conn().query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap();
    assert_eq!(rows, 1, "撤销是账不是删行（与 retirement 的既有形状同律）");
    let acct: i64 = m.conn().query_row(
        "SELECT count(*) FROM retirement WHERE mem_table='fact_link' AND mem_id=?1 AND resolved_at_ms IS NULL",
        [&link], |r| r.get(0)).unwrap();
    assert_eq!(acct, 1, "撤销账必须落在 retirement 的唯一形状里");
}

/// R3：`event_link` 是写时间线时的旁路写入。`put_timeline_rows_counted` 必须把它单独交出来
/// （derive 侧并进 `rows_written` ⇒ `derive_rows` 读数可见）；旧入口的返回值不变。
#[test]
fn counted_variant_reports_the_event_link_bypass_write() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let meeting = put_event(m, &FactContent {
        kind: "meeting".into(), title: "季度会".into(), body: "会上定了十月发布。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('a'), NOW).unwrap();
    let basis = put_card(m, &FactContent {
        kind: "decision".into(), title: "发布决定".into(), body: "十月发布。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('b'), NOW).unwrap();
    let (rows, links) = put_timeline_rows_counted(
        m, &[timeline("发布排在季度会之后。", &meeting.id, vec![basis.id.clone()])],
        &BatchCtx::synthetic(), NOW).unwrap();
    assert_eq!((rows, links), (1, 1), "1 条时间行 + 1 条旁路软边");
    let n: i64 = m.conn().query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
    // 旧入口语义不变：只报时间行数。
    let again = put_timeline_rows(
        m, &[timeline("第二条时间行。", &meeting.id, vec![])], &BatchCtx::synthetic(), NOW).unwrap();
    assert_eq!(again, 1, "put_timeline_rows 的返回值仍是时间行数（不含软边）");
}

/// I3：无锚时间线的 `as_of_ms` / `created_at_ms` 取依据行的源侧时刻，不取导入钟。
#[test]
fn an_unanchored_timeline_takes_source_time_from_its_basis_row_not_now() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let basis = put_card(m, &FactContent {
        kind: "decision".into(), title: "依据行".into(), body: "十月发布。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('c'), 1_234).unwrap();
    put_timeline_rows(m, &[TimelineContent {
        title: "derive-无锚".into(), body: "无锚时间行".into(), scope: scope(),
        time_anchor_ms: None, time_anchor_id: None, time_confidence: "unknown".into(),
        basis_ids: vec![basis.id.clone()],
    }], &BatchCtx::synthetic(), 999_999).unwrap();
    let (as_of, created, anchor): (i64, i64, Option<i64>) = m.conn().query_row(
        "SELECT as_of_ms, created_at_ms, time_anchor_ms FROM card WHERE kind='timeline'", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!((as_of, created, anchor), (1_234, 1_234, None),
        "as_of/created_at 取依据行时刻；锚列仍诚实留 NULL");
}
