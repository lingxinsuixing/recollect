//! 债 #9（修法甲）：`card.time_json` 里真有事件时间（`kind='event_time'`）的卡，
//! 四个消费面必须给事件时间、不得再说发话钟；没有这一格的卡维持 BASE 字节。
//!
//! 钉四格：
//! 1. brief / card / expand 的事件支措辞（`这条记忆说的事发生于 …` / `它说的事发生于 …`）；
//! 2. `view=json` 的新行键 `event_time_ms`（`as_of_ms` 值与列语义一字不动）；
//! 3. hook 注入行的事件支（`as_of=<列值>（事件发生于 …）`，机读键不动）与导出件的
//!    `time=事件发生于 …` 格；
//! 4. 边界：`due`/`valid` 两族不触发事件支；事件支优先于 `speech` 支；无 `time_json`
//!    的卡渲染与 BASE 同字节（逐字节快照的读者是 `recall_default_bytes.rs`，这里只断
//!    支臂选择）。
//!
//! 夹具的 `time_json` 手写 SQL（写侧 `pass_time` 是禁改面；JSON 形状逐格照抄
//! `card_time_json` 的五键，见 `rc-memory/tests/card_event_time.rs`）。

use rc_agent::recall::{
    recall_with, recall_narrative_with, RecallFilter, RecallOpts, RecallRequest, Intent,
    NarrativeView,
};
use rc_agent::surface::{probe_block, probe_via_search, FENCE};
use rc_agent::{render_export, ExportRequest};
use rc_contract::text::{format_cn_date, format_cn_ms};
use rc_memory::keys::{AnchorRole, MemKey, Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_episodes, BatchCtx, EpisodeContent, FactContent};
use rc_memory::{put_fact_rows_with_extras, FactExtras, FactWriteItem, Provenance};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;
/// 事件时间：早于发话钟两个小时——这样"事件支真的换了钟"才可判（同一个数就分不出两支）。
const EV: i64 = NOW - 2 * 60 * MIN;
const Q: &str = "事件时间读者面合成词";
const TZ: i32 = 480;

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-event".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx::synthetic()
}

fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

struct Fix {
    _dir: tempfile::TempDir,
    root: Root,
}

impl Fix {
    fn m(&self) -> &rc_store::MemoryHandle {
        self.root.memory()
    }
}

fn fix() -> Fix {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    Fix { _dir: dir, root }
}

/// 一张普通卡（`put_card` ⇒ `time_confidence='unknown'`、`time_json` NULL），
/// 返回 `card:<id>` 句柄。`title` 自带查询词 ⇒ 句柄臂与查询臂都命中它。
fn card(fix: &Fix, c: char, title: &str, body: &str) -> String {
    let k = put_card(fix.m(), &FactContent {
        kind: "event".into(), title: title.into(), body: body.into(),
        scope: scope(), valid_to_ms: None,
    }, &prov(c), NOW).unwrap();
    k.key_string()
}

/// 把一张卡的事件钟写上（`kind='event_time'`，形状与 `pass_time::card_time_json` 逐格一致）。
fn set_event(fix: &Fix, handle: &str, start: i64) {
    let id = handle.split_once(':').map(|(_, i)| i).unwrap_or(handle);
    fix.m().conn().execute_batch(&format!(
        "UPDATE card SET time_json='{{\"kind\":\"event_time\",\"field\":\"occurred\",\
         \"start_ms\":{start},\"end_ms\":null,\"precision\":\"minute\",\"basis\":\"explicit\"}}' \
         WHERE id='{id}'")).unwrap();
}

fn by_handle(fix: &Fix, handle: &str, opts: &RecallOpts) -> Value {
    let req = RecallRequest::Handles(vec![MemKey::parse(handle).unwrap()]);
    let text = recall_with(fix.m(), &req, &RecallFilter::empty(), NOW, opts).unwrap().text();
    let doc: Value = serde_json::from_str(&text).unwrap();
    doc["rows"][0].clone()
}

fn narrative(fix: &Fix, handle: &str, view: NarrativeView, opts: &RecallOpts) -> String {
    let req = RecallRequest::Handles(vec![MemKey::parse(handle).unwrap()]);
    recall_narrative_with(fix.m(), &req, &RecallFilter::empty(), NOW, view, opts).unwrap().text()
}

// ── 1. 三个自然语言视图的事件支 ────────────────────────────────────────────────

#[test]
fn brief_与_expand_的事件支用事件钟说话() {
    let fix = fix();
    let h = card(&fix, 'a', &format!("{Q}的第一件"), "发布流程改了。");
    set_event(&fix, &h, EV);
    let brief = narrative(&fix, &h, NarrativeView::Brief, &RecallOpts::default());
    let stamp = format_cn_date(EV, TZ);
    assert!(brief.contains(&format!("这条记忆说的事发生于 {stamp}")),
        "brief 没说事件钟：\n{brief}");
    assert!(!brief.contains("记录于"), "有事件时间还走记录于那一支：\n{brief}");
    assert!(!brief.contains("发话于"), "有事件时间却说发话钟：\n{brief}");

    let expand = narrative(&fix, &h, NarrativeView::Expand, &RecallOpts::default());
    let full = format_cn_ms(EV, TZ);
    assert!(expand.contains(&format!("它说的事发生于 {full}")),
        "expand 没说事件钟：\n{expand}");
    assert!(!expand.contains("记录于") && !expand.contains("发话于"), "\n{expand}");
}

/// 未来向的事件时间走同一支、只换相对读数（`距离现在…`），不落回"记录于"。
#[test]
fn 未来向的事件时间_相对读数是距离现在() {
    let fix = fix();
    let h = card(&fix, 'f', &format!("{Q}的未来件"), "计划中的评审。");
    set_event(&fix, &h, NOW + 3 * 60 * MIN);
    let expand = narrative(&fix, &h, NarrativeView::Expand, &RecallOpts::default());
    let full = format_cn_ms(NOW + 3 * 60 * MIN, TZ);
    assert!(expand.contains(&format!("它说的事发生于 {full}，距离现在")),
        "未来向事件支的相对读数不对：\n{expand}");
}

// ── 2. view=json 的新键 ───────────────────────────────────────────────────────

#[test]
fn json_事件键在场而_as_of_ms_列语义不动() {
    let fix = fix();
    let h = card(&fix, 'b', &format!("{Q}的第二件"), "机读面那一格。");
    set_event(&fix, &h, EV);
    let row = by_handle(&fix, &h, &RecallOpts::default());
    // 逐格点断言（不许只数键个数）：事件键的值就是 `time_json` 的 `start_ms`。
    assert_eq!(row["event_time_ms"], json!(EV.to_string()),
        "event_time_ms 不是 time_json 的 start_ms：{row}");
    // `as_of_ms` 的值与语义一字不动：它始终是行自带的那口钟（这一格 = 源轮时刻）。
    assert_eq!(row["as_of_ms"], json!(NOW.to_string()),
        "as_of_ms 被事件钟顶掉了 ⇒ 列语义被改：{row}");
    assert_eq!(row["time_confidence"], json!("unknown"),
        "time_confidence 仍是列的归一读数，不掺事件支：{row}");
}

/// 无 `time_json` 的卡：事件键恒 null，文本走既有支（brief 的逐字节快照另有
/// `recall_default_bytes.rs` 钉着，这里断支臂选择与键的缺省值）。
#[test]
fn 无time_json的卡_事件键为null_文本走既有支() {
    let fix = fix();
    let h = card(&fix, 'c', &format!("{Q}的第三件"), "没有 time_json。");
    let row = by_handle(&fix, &h, &RecallOpts::default());
    assert!(row["event_time_ms"].is_null(), "没写 time_json ⇒ 事件键恒 null：{row}");
    let text = narrative(&fix, &h, NarrativeView::Expand, &RecallOpts::default());
    assert!(text.contains("它记录于 "), "无 time_json 必须走既有那一支：\n{text}");
    assert!(!text.contains("发生于"), "无 time_json 不许出现事件支措辞：\n{text}");
}

// ── 3. 边界：due/valid 不触发；事件支优先于 speech ────────────────────────────

#[test]
fn due_与_valid_两族不触发事件支() {
    let fix = fix();
    let h = card(&fix, 'd', &format!("{Q}的截止件"), "deadline 那一族。");
    let id = h.split_once(':').map(|(_, i)| i).unwrap_or(&h).to_string();
    fix.m().conn().execute_batch(&format!(
        "UPDATE card SET time_json='{{\"kind\":\"deadline\",\"field\":\"due\",\
         \"start_ms\":{EV},\"end_ms\":null,\"precision\":\"minute\",\"basis\":\"explicit\"}}' \
         WHERE id='{id}'")).unwrap();
    let row = by_handle(&fix, &h, &RecallOpts::default());
    assert!(row["event_time_ms"].is_null(), "due 族不是事件时间：{row}");
    let text = narrative(&fix, &h, NarrativeView::Expand, &RecallOpts::default());
    assert!(text.contains("它记录于 "), "due 族走既有支：\n{text}");
}

#[test]
fn 事件支优先于speech支() {
    let fix = fix();
    let h = card(&fix, 'e', &format!("{Q}的两钟卡"), "speech 列与 time_json 并存。");
    let id = h.split_once(':').map(|(_, i)| i).unwrap_or(&h).to_string();
    set_event(&fix, &h, EV);
    fix.m().conn().execute_batch(&format!(
        "UPDATE card SET time_confidence='speech' WHERE id='{id}'")).unwrap();
    let row = by_handle(&fix, &h, &RecallOpts::default());
    assert_eq!(row["time_confidence"], json!("speech"), "列读数照旧（不掺事件支）：{row}");
    assert_eq!(row["event_time_ms"], json!(EV.to_string()), "{row}");
    let text = narrative(&fix, &h, NarrativeView::Expand, &RecallOpts::default());
    assert!(text.contains("它说的事发生于 "), "有真事件时间必须用事件钟：\n{text}");
    assert!(!text.contains("发话于") && !text.contains("事件时间未知"),
        "契约 1：有 time_json 事件时间的卡不得再说发话钟/事件时间未知：\n{text}");
}

// ── 4. hook 注入面与导出件 ────────────────────────────────────────────────────

#[test]
fn 注入行的事件支_as_of键与值不动_补语说事件钟() {
    let fix = fix();
    let h = card(&fix, 'g', &format!("{Q}的注入件"), "注入面那一行。");
    set_event(&fix, &h, EV);
    rc_index::rebuild(fix.m(), NOW).unwrap();
    let view = probe_via_search(fix.m(), None, Q, NOW).unwrap();
    let key = MemKey::parse(&h).unwrap();
    let d = view.forms.iter().find(|x| x.key == key).expect("那张卡在候选面上");
    let line = probe_block(&view).text().lines()
        .find(|l| l.contains(&key.key_string())).expect("注入块里有这一行").to_string();
    assert_eq!(line,
        format!("{FENCE}{} | {} | as_of={}（事件发生于 {}）",
                d.key.key_string(), d.facts.title, d.facts.as_of_ms,
                format_cn_ms(EV, TZ)),
        "注入行事件支的字节账：{line}");
}

/// 同一张卡没有 `time_json` 时，注入行与 BASE 逐字相同（`surface_time_labels.rs`
/// 的 unknown/NULL 两支是这套字节的读者，这里再钉一张带查询词的 event 卡）。
#[test]
fn 注入行_无time_json_与_base_逐字相同() {
    let fix = fix();
    let h = card(&fix, 'h', &format!("{Q}的素注入件"), "没有 time_json 的注入行。");
    rc_index::rebuild(fix.m(), NOW).unwrap();
    let view = probe_via_search(fix.m(), None, Q, NOW).unwrap();
    let key = MemKey::parse(&h).unwrap();
    let d = view.forms.iter().find(|x| x.key == key).expect("那张卡在候选面上");
    let line = probe_block(&view).text().lines()
        .find(|l| l.contains(&key.key_string())).expect("注入块里有这一行").to_string();
    assert_eq!(line,
        format!("{FENCE}{} | {} | as_of={}", d.key.key_string(), d.facts.title, d.facts.as_of_ms),
        "无 time_json 的注入行不许漂一个字节：{line}");
}

#[test]
fn 导出件的事件支_与_speech_格同槽() {
    let fix = fix();
    let h = card(&fix, 'i', &format!("{Q}的导出件"), "导出面那一行。");
    set_event(&fix, &h, EV);
    let doc = render_export(fix.m(), &ExportRequest::all(NOW).unwrap()).unwrap().text();
    let line = doc.lines().find(|l| l.contains(&h)).expect("那张卡在导出件里");
    assert!(line.contains(&format!("time=事件发生于 {}", format_cn_ms(EV, TZ))),
        "导出件没给事件钟：\n{line}");
    // 无 time_json 的另一张：行尾一格都不加（既有 diff 面）。
    let plain = card(&fix, 'j', &format!("{Q}的素导出件"), "没有 time_json 的导出行。");
    let doc2 = render_export(fix.m(), &ExportRequest::all(NOW).unwrap()).unwrap().text();
    let line2 = doc2.lines().find(|l| l.contains(&plain)).expect("素卡在导出件里");
    assert!(!line2.contains("time="), "无 time_json 的行不许加格：\n{line2}");
}

// ── 5. L2 经历块：镜像卡自己的 time_json ─────────────────────────────────────

/// 一条真 fact（自带陈述面消费卡 + 归属行），形状照 `recall_layer.rs` 的夹具；
/// 主语锚由调用方先 INSERT 进 `anchor` 表（fact 的主语必须存在于 `anchor`）。
fn fact(fix: &Fix, index: usize, value: &str, speech_ms: i64) -> String {
    let subject: String = fix.m().conn().query_row(
        "SELECT id FROM anchor ORDER BY created_at_ms, id LIMIT 1", [], |r| r.get(0)).unwrap();
    let item = FactWriteItem {
        subject_anchor_id: subject,
        object_anchor_id: None,
        predicate: "做了".into(),
        value: Some(value.into()),
        category: "event".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: value.into(),
        evidence: vec![(format!("{:064x}", 0x51 + index), AnchorRole::Human)],
        text: format!("甲某{value}"),
        scope: scope(),
        source_ms: speech_ms,
    };
    let extras = FactExtras { participants: vec![], ..Default::default() };
    let out = put_fact_rows_with_extras(fix.m(), &[item], &[extras], &ctx(),
                                        &owner(), NOW).unwrap();
    assert_eq!(out.facts, 1, "夹具的 fact 必须真的落库：{value}");
    format!("card:{}", out.card_ids[0].id)
}

fn owner() -> rc_memory::owner::OwnerCtx {
    rc_memory::owner::OwnerCtx {
        owner_principal_id: Some("p-event".into()),
        owner_agent_id: "synthetic-agent".into(),
        visibility: rc_memory::owner::Visibility::Project,
        retention: "keep".into(),
    }
}

/// 从卡的 `pointers_json` 反查它点名的第一条 fact id。
fn first_fact_id(fix: &Fix, handle: &str) -> String {
    let id = handle.split_once(':').map(|(_, i)| i).unwrap_or(handle);
    let json: String = fix.m().conn().query_row(
        "SELECT pointers_json FROM card WHERE id=?1", [id], |r| r.get(0)).unwrap();
    for part in json.trim_matches(|c| c == '[' || c == ']').split(',') {
        if let Some(f) = part.trim().trim_matches('"').strip_prefix("fact:") {
            return f.to_string();
        }
    }
    panic!("夹具卡必须点回一条 fact：{handle} / {json}");
}

/// 一段两成员的经历（成员无事件时间 ⇒ 镜像卡落 `speech` 支的真生产者形状），
/// 再给**镜像卡自己**写上 `time_json` 事件时间——判据认列不认卡族的那一格。
#[test]
fn 经历块_镜像卡有time_json事件时间时说事件钟() {
    let fix = fix();
    fix.m().conn().execute_batch(&format!(
        "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                            status, created_at_ms, updated_at_ms, norm_version)
         VALUES('{:064x}', 'person', '甲某', 'real', 'project', 'proj-event',
                 'active', {NOW}, {NOW}, 1)", 0xc0u64)).unwrap();
    let a = fact(&fix, 0, "调通闸", NOW);
    let b = fact(&fix, 1, "补了桩", NOW + MIN);
    let mut fact_ids = Vec::new();
    for h in [&a, &b] {
        fact_ids.push(first_fact_id(&fix, h));
    }
    put_episodes(fix.m(), &[EpisodeContent {
        title: format!("{Q}的那段经历"),
        narrative: "先调通闸，再补了桩。".into(),
        fact_ids,
    }], &ctx(), NOW).unwrap();
    let mirror_id: String = fix.m().conn().query_row(
        "SELECT id FROM card WHERE kind='episode' ORDER BY created_at_ms, id LIMIT 1",
        [], |r| r.get(0)).unwrap();
    set_event(&fix, &format!("card:{mirror_id}"), EV);
    let handle = format!("card:{mirror_id}");
    let opts = RecallOpts { intent: Intent::When, ..Default::default() };
    let row = by_handle(&fix, &handle, &opts);
    assert_eq!(row["time_confidence"], json!("speech"),
        "夹具的镜像卡走 speech 那一支（成员全无事件时间）");
    let expand = narrative(&fix, &handle, NarrativeView::Expand, &opts);
    let header = expand.lines().find(|l| l.contains("它所属的经历（L2）："))
        .expect("expand 面上有经历块");
    assert!(header.contains(&format!("事件时间 {}", format_cn_ms(EV, TZ))),
        "镜像卡自己的 time_json 事件时间没进经历块：\n{expand}");
    assert!(!header.contains("发话于"),
        "有真事件时间的经历块不得再说发话钟：{header}");
    // 成员行照旧按各自的时间档说话（成员没有事件时间 ⇒ speech 支合法），不在此格的断言内。
}
