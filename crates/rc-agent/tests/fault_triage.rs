// crates/rc-agent/tests/fault_triage.rs
//! I-10（B4 故障可分诊）的三臂夹具：**故障 / 闸拒 / 真没有**在读侧互相可分。
//!
//! 派发档的两条硬边界在这里各有一组牙：
//! ① **真故障必须以错误形态暴露**——夹具注入库级故障（损坏 `memory_mention` 表、
//!    写坏 `evidence_json` 的 JSON），召回面必须回 `unavailable:store_unreachable` 一族，
//!    不许落干净的 `no_rows` / `no_edges`；
//! ② **闸拒在消费面维持"被挡与不存在同形"**——`unresolved_handles` / `empty:row_absent`
//!    一字不动；分诊计数只进显式 opt-in（`RecallOpts.deny_diag` + `view=json`），
//!    默认视图没有 `gate_deny_diag` 键（连空对象都没有）。计数**逐格**给
//!    （八个 Deny 原因键零填充在场），不许只给总数。
//!
//! 夹具抄 `tests/recall_ownership.rs`：`put_batch` 以 agent-A / agent-B 各写一张卡、
//! 共用同一查询词；归属行随行落库。全部断真出口 `rc_agent::recall::{recall_with,
//! recall_narrative_with}`，不自造 SQL 尺。

use rc_agent::recall::{
    recall_narrative_with, recall_with, NarrativeView, RecallFilter, RecallOpts, RecallRequest,
    Viewer,
};
use rc_agent::{AgentErrorKind, ToolError};
use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::writers_x::{put_batch, put_entity, BatchCtx, EntityContent, WriteItem};
use rc_memory::{Provenance, Scope as MemScope};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;
/// 两张卡共用的查询词：命中它就是命中两行，读者差异只来自读闸。
const QUERY: &str = "故障分诊合成词";
/// 库里不存在的一张卡（合法形状的幽灵句柄）。
const GHOST: &str = "card:000000000000000000000000000000000000000000000000000000000000abcdef";

fn ctx(agent: &str) -> BatchCtx {
    BatchCtx { source_agent: agent.into(), device_id: format!("dev-{agent}") }
}

fn card(title: &str) -> WriteItem {
    WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: title.into(),
        body: format!("{QUERY} 的一句合成正文"),
        scope: Scope { kind: ScopeKind::Project, key: "proj-triage".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }],
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: Root,
    a: String,
    b: String,
}

/// 同库两行：A 署名一行、B 署名一行（private_agent 归属由 `put_batch` 的合成默认给出）。
fn seeded() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = put_batch(m, &[card("分诊甲卡")], &ctx("agent-A"), NOW).unwrap()[0]
        .clone().expect("A 的行不被抑制");
    let b = put_batch(m, &[card("分诊乙卡")], &ctx("agent-B"), NOW).unwrap()[0]
        .clone().expect("B 的行不被抑制");
    assert_ne!(a, b, "前置：两行的句柄必须不同（内容寻址含标题）");
    rc_index::rebuild(m, NOW).unwrap();
    Fixture { _dir: dir, root, a: a.key_string(), b: b.key_string() }
}

fn filter(viewer: Option<Viewer>) -> RecallFilter {
    RecallFilter { viewer, ..Default::default() }
}

fn opts(deny_diag: bool) -> RecallOpts {
    RecallOpts { deny_diag, ..Default::default() }
}

/// `view=json`、句柄臂的一次调用（成功时返回 JSON 文档）。
fn handles_json(
    fx: &Fixture, handles: &[&str], viewer: Option<Viewer>, o: &RecallOpts,
) -> Result<Value, ToolError> {
    let raw: Vec<String> = handles.iter().map(|s| s.to_string()).collect();
    let (req, dropped) = RecallRequest::handles(&raw).unwrap();
    assert_eq!(dropped, 0, "前置：夹具句柄都是合法形状，不该有条被拒");
    let json = recall_with(fx.root.memory(), &req, &filter(viewer), NOW, o)?.text();
    serde_json::from_str(&json).map_err(|e| panic!("view=json 必须是 JSON：{e}"))
}

fn query_json(
    fx: &Fixture, viewer: Option<Viewer>, o: &RecallOpts,
) -> Result<Value, ToolError> {
    let req = RecallRequest::query(QUERY).unwrap();
    let json = recall_with(fx.root.memory(), &req, &filter(viewer), NOW, o)?.text();
    serde_json::from_str(&json).map_err(|e| panic!("view=json 必须是 JSON：{e}"))
}

fn rows_of(v: &Value) -> Vec<String> {
    v["rows"].as_array().expect("rows 必须是数组")
        .iter().map(|r| r["id"].as_str().expect("每行都带句柄").to_owned()).collect()
}

fn unresolved_of(v: &Value) -> Vec<String> {
    v["unresolved_handles"].as_array().expect("unresolved_handles 必须是数组")
        .iter().map(|x| x.as_str().expect("每一项都是字符串").to_owned())
        .filter(|s| !s.starts_with('+'))
        .collect()
}

/// 八个原因格的零填充在场 + 逐格取值。**键集必须恰是八格**：多一格（漂移进别的计数）
/// 与少一格（断言没法逐格点名）都红。
fn reason_of(diag: &Value, slug: &str) -> i64 {
    let by = diag["by_reason"].as_object().expect("by_reason 必须是对象");
    assert_eq!(by.len(), 8, "by_reason 的键集必须恰是八个 Deny 原因格：{by:?}");
    by[slug].as_i64().unwrap_or_else(|| panic!("原因格 {slug} 必须在场且是整数：{by:?}"))
}

fn assert_all_zero_except(diag: &Value, only: &str) {
    assert_eq!(diag["denied_total"], json!(1), "恰好一次拒绝：{diag}");
    for slug in ["no_owner_row", "private_agent", "person_needs_grant", "scope_mismatch",
                 "org_unsupported", "grant_missing", "grant_expired", "table_missing"] {
        let want = if slug == only { 1 } else { 0 };
        assert_eq!(reason_of(diag, slug), want, "原因格 {slug} 的计数不对：{diag}");
    }
}

// ── 闸拒臂：opt-in 按原因逐格计数；消费面同形不变 ─────────────────────────────────

/// 句柄臂：A 点名 B 的行（被拒）+ 一个幽灵句柄（真没有）。
/// 消费面：两者都进 `unresolved_handles`，与 BASE 同形；诊断面：`private_agent=1`、
/// `handle_absent=1`、其余七格 0——三态在这一份读数里互相可分。
#[test]
fn 闸拒句柄臂_opt_in_按原因逐格计数_消费面同形不变() {
    let fx = seeded();
    let v = handles_json(&fx, &[&fx.b, GHOST], Some(Viewer::agent("agent-A")), &opts(true))
        .expect("句柄臂不是空读数（absent 非空），必须 Ok");
    // 消费面：同形那一格一个字节都不因 opt-in 改变。
    assert!(rows_of(&v).is_empty(), "被拒句柄不许带回正文行：{v}");
    assert_eq!(unresolved_of(&v), vec![fx.b.clone(), GHOST.to_string()],
        "被拒与查不到在消费面上必须同形：{v}");
    // 诊断面：逐格计数。
    let diag = &v["gate_deny_diag"];
    assert!(diag.is_object(), "opt-in 时 gate_deny_diag 必须在场：{v}");
    assert_eq!(diag["handle_absent"], json!(1), "幽灵句柄算真没有，不算拒绝：{diag}");
    assert_all_zero_except(diag, "private_agent");
}

/// 默认（不传 deny_diag）：返回里**没有** `gate_deny_diag` 键——
/// 默认视图与 BASE 逐字节相同那一格的键集形状（字节对照由 `recall_default_bytes.rs` 担保）。
#[test]
fn 默认视图不带诊断键() {
    let fx = seeded();
    let v = handles_json(&fx, &[&fx.b, GHOST], Some(Viewer::agent("agent-A")), &opts(false))
        .expect("句柄臂不是空读数，必须 Ok");
    assert!(v.get("gate_deny_diag").is_none(),
        "缺省读数不许冒出诊断键：{v}");
    // 键集与"干净的两次同形调用"逐键相同。G1 刀 A 起顶层是六个键：原 BASE 五键
    // 之外多一个**无条件**的 `value_mix` 信封（派发档裁定⑥，不是 opt-in 诊断键）；
    // 精确键清单由 `recall_default_bytes.rs` 担保，这里只钉"诊断键缺席 + 总数不漂"。
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys.len(), 6, "顶层键集不许漂：{keys:?}");
    assert!(v.get("value_mix").is_some(), "value_mix 是无条件信封键，缺省也在场：{v}");
}

/// `no_owner_row` 逐格：删掉 B 那行的归属行，A 再点名它——
/// `decide` 第 3 条（无归属行且署名≠viewer）落在 `no_owner_row`，不许并进 `private_agent`。
#[test]
fn no_owner_row_逐格() {
    let fx = seeded();
    let id = fx.b.strip_prefix("card:").unwrap();
    fx.root.memory().conn().execute(
        "DELETE FROM resource_owner WHERE mem_table='card' AND mem_id=?1", [id]).unwrap();
    let v = handles_json(&fx, &[&fx.b], Some(Viewer::agent("agent-A")), &opts(true))
        .expect("句柄臂不是空读数，必须 Ok");
    assert_eq!(unresolved_of(&v), vec![fx.b.clone()], "消费面同形不变：{v}");
    assert_all_zero_except(&v["gate_deny_diag"], "no_owner_row");
}

/// 查询臂部分闸拒：命中两行、A 只看得见自己那行——
/// 可见行照常返回（不换码、不报错），被拒的那行按原因进计数。
#[test]
fn 查询臂部分闸拒_按原因计数() {
    let fx = seeded();
    let v = query_json(&fx, Some(Viewer::agent("agent-A")), &opts(true))
        .expect("部分可见不许报错");
    assert_eq!(rows_of(&v), vec![fx.a.clone()], "可见行集不许变：{v}");
    let diag = &v["gate_deny_diag"];
    assert_eq!(diag["handle_absent"], json!(0), "查询臂没有句柄缺席这一格：{diag}");
    assert_all_zero_except(diag, "private_agent");
}

/// `scope_mismatch` 逐格：把 B 那行改成 `visibility='project'`——
/// viewer A 没声明 scope ⇒ `scope_eq` 为假 ⇒ 落 `scope_mismatch`，不许并进 `private_agent`。
#[test]
fn scope_mismatch_逐格() {
    let fx = seeded();
    let id = fx.b.strip_prefix("card:").unwrap();
    fx.root.memory().conn().execute(
        "UPDATE resource_owner SET visibility='project' WHERE mem_table='card' AND mem_id=?1",
        [id]).unwrap();
    let v = handles_json(&fx, &[&fx.b], Some(Viewer::agent("agent-A")), &opts(true))
        .expect("句柄臂不是空读数，必须 Ok");
    assert_all_zero_except(&v["gate_deny_diag"], "scope_mismatch");
}

/// viewer=None（中心 CLI/旧调用）：不过闸 ⇒ 没有拒绝可数；
/// 幽灵句柄仍按真没有报 `handle_absent`（这与闸无关，是"库里没有"的如实读数）。
#[test]
fn viewer_none_不过闸_诊断只有真没有那一格() {
    let fx = seeded();
    let v = handles_json(&fx, &[&fx.a, &fx.b, GHOST], None, &opts(true))
        .expect("viewer=None 的句柄臂旧行为：点名谁给谁");
    assert_eq!(rows_of(&v), vec![fx.a.clone(), fx.b.clone()],
        "viewer=None 的旧行为被闸住了");
    let diag = &v["gate_deny_diag"];
    assert_eq!(diag["denied_total"], json!(0), "不过闸就没有拒绝：{diag}");
    assert_eq!(diag["handle_absent"], json!(1), "幽灵句柄是真没有：{diag}");
    for slug in ["no_owner_row", "private_agent", "person_needs_grant", "scope_mismatch",
                 "org_unsupported", "grant_missing", "grant_expired", "table_missing"] {
        assert_eq!(reason_of(diag, slug), 0, "原因格 {slug} 在不过闸时必须为 0：{diag}");
    }
}

// ── 消费面同形的两格原样（opt-in 不改变错误面） ──────────────────────────────────

/// expand 面的被拒句柄仍是 `empty:row_absent`（与"查不到"同形，D5 那格不拆）。
#[test]
fn expand_面被拒仍是_row_absent_同形不变() {
    let fx = seeded();
    let raw = vec![fx.b.clone()];
    let (req, _) = RecallRequest::handles(&raw).unwrap();
    let err = recall_narrative_with(
        fx.root.memory(), &req, &filter(Some(Viewer::agent("agent-A"))), NOW,
        NarrativeView::Expand, &opts(false),
    ).expect_err("被拒的 expand 句柄必须仍是既有错误形态");
    assert_eq!((err.kind, err.code), (AgentErrorKind::Empty, "row_absent"),
        "expand 的同形格不许因 B4 改变：{err:?}");
}

/// 叙述面显式拒绝 deny_diag（`validation/deny_diag_json_only`）——
/// 不许"悄悄忽略 opt-in"让调用方误以为拿到了诊断。
#[test]
fn 叙述面拒绝_deny_diag_json_only() {
    let fx = seeded();
    let req = RecallRequest::query(QUERY).unwrap();
    for view in [NarrativeView::Brief, NarrativeView::Card, NarrativeView::Expand] {
        let err = recall_narrative_with(
            fx.root.memory(), &req, &filter(Some(Viewer::agent("agent-A"))), NOW, view,
            &opts(true),
        ).expect_err("叙述面的 deny_diag 必须显式拒绝");
        assert_eq!((err.kind, err.code), (AgentErrorKind::Validation, "deny_diag_json_only"),
            "{view:?} 没有拒绝 deny_diag：{err:?}");
    }
}

// ── 真没有臂：诊断如实报 0 拒绝 ─────────────────────────────────────────────────

#[test]
fn 真没有臂_诊断报零拒绝_空库仍是_no_rows() {
    // 句柄臂：幽灵句柄 ⇒ `handle_absent=1`、八个原因格全 0——"真没有"与"被拒"在诊断面可分。
    let fx = seeded();
    let v = handles_json(&fx, &[GHOST], Some(Viewer::agent("agent-A")), &opts(true))
        .expect("句柄臂不是空读数，必须 Ok");
    assert_eq!(rows_of(&v).len(), 0);
    let diag = &v["gate_deny_diag"];
    assert_eq!(diag["denied_total"], json!(0), "真没有不许报出拒绝：{diag}");
    assert_eq!(diag["handle_absent"], json!(1), "缺席数要如实：{diag}");
    for slug in ["no_owner_row", "private_agent", "person_needs_grant", "scope_mismatch",
                 "org_unsupported", "grant_missing", "grant_expired", "table_missing"] {
        assert_eq!(reason_of(diag, slug), 0, "原因格 {slug} 必须为 0：{diag}");
    }

    // 库真空 + opt-in：查询臂仍走既有的 `empty:no_rows`（错误面一字不动）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
    let req = RecallRequest::query(QUERY).unwrap();
    let err = recall_with(root.memory(), &req,
        &filter(Some(Viewer::agent("agent-C"))), NOW, &opts(true))
        .expect_err("空库该得到空读数错误");
    assert_eq!((err.kind, err.code), (AgentErrorKind::Empty, "no_rows"),
        "空库的分型一字不动：{err:?}");
}

// ── 故障臂：库级故障必须以错误形态暴露，不许装成"没有" ────────────────────────────

/// 损坏 `memory_mention` 表（DROP）：`recall_filter_match` 的 mention 支取数失败——
/// 旧形状会把它吞成"没提到该实体"⇒ 干净 `no_rows`；现在必须以
/// `unavailable:store_unreachable` 暴露（F2）。
#[test]
fn 故障臂_mention_表损坏_召回以错误暴露_不装成_no_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 一条实体（名字可解析）+ 一条命中查询词的卡：entity 过滤解析出非空 id 集，
    // 才会真走到 `row_mentions_any` 那条故障支。
    let scope = MemScope { kind: ScopeKind::Project, key: "proj-triage".into() };
    let prov = vec![Provenance { turn_uid: "f".repeat(64), anchor_role: AnchorRole::Human }];
    put_entity(m, &EntityContent {
        kind: "person".into(), title: "分诊实体".into(), scope: scope.clone(),
        aliases: vec![],
    }, &prov, NOW).unwrap();
    put_batch(m, &[card("故障臂卡")], &ctx("agent-A"), NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    m.conn().execute("DROP TABLE memory_mention", []).unwrap();

    let req = RecallRequest::query(QUERY).unwrap();
    let flt = RecallFilter {
        entity: Some("分诊实体".into()),
        viewer: Some(Viewer::agent("agent-A")),
        ..Default::default()
    };
    let err = recall_with(m, &req, &flt, NOW, &opts(true))
        .expect_err("库级故障必须以错误读数暴露，不许装成空");
    assert_eq!((err.kind, err.code), (AgentErrorKind::Unavailable, "store_unreachable"),
        "故障臂的分型：{err:?}");
    assert!(err.hint.contains("doctor"), "故障的下一步是诊断：{err}");
}

/// `evidence_json` 写坏（非法 JSON）且 `basis` 为空：这条边在旧形状下会被
/// "证据两格都空"判据整条吞掉——干净到连 `no_edges` 都不报；现在必须以错误暴露（F3）。
#[test]
fn 故障臂_坏_evidence_json_边不吞成_没有() {
    let fx = seeded();
    // 一条真边（basis 留空、evidence_json 写坏）：旧判据 `reason.is_empty() && evidence.is_empty()`
    // 要先解 JSON 才知道 evidence 空——解析失败就是故障，不是"没有证据"。
    fx.root.memory().conn().execute_batch(&format!(
        "INSERT INTO fact_link(id, from_id, to_id, relation, basis, evidence_json, decided_by,
                               scope_kind, scope_key, created_at_ms, norm_version,
         source_agent, device_id)
         VALUES('edge-bad-json', 'card:{}', 'card:{}', 'depends_on',
                '', '{{bad json', NULL, 'project', 'proj-triage', {NOW}, 1,
                'agent-writer', 'dev-triage')",
        fx.a.strip_prefix("card:").unwrap(), fx.b.strip_prefix("card:").unwrap(),
    )).unwrap();

    let raw = vec![fx.a.clone()];
    let (req, _) = RecallRequest::handles(&raw).unwrap();
    let o = RecallOpts { include_edges: true, deny_diag: true, ..Default::default() };
    let err = recall_with(fx.root.memory(), &req, &filter(Some(Viewer::agent("agent-A"))),
        NOW, &o).expect_err("坏 JSON 是库数据故障，必须以错误读数暴露");
    assert_eq!((err.kind, err.code), (AgentErrorKind::Unavailable, "store_unreachable"),
        "故障臂的分型：{err:?}");
}

/// 反向对照：合法的空证据 `'[]'` + 空 basis 的边**不是**故障——既有"两格都空不渲染"的
/// 形状一字不动（B4 只把**解析失败**升为故障，不许把宽容面一起收掉）。
#[test]
fn 合法空证据_两格都空的边仍不渲染_不是故障() {
    let fx = seeded();
    fx.root.memory().conn().execute_batch(&format!(
        "INSERT INTO fact_link(id, from_id, to_id, relation, basis, evidence_json, decided_by,
                               scope_kind, scope_key, created_at_ms, norm_version,
         source_agent, device_id)
         VALUES('edge-empty', 'card:{}', 'card:{}', 'depends_on',
                '', '[]', NULL, 'project', 'proj-triage', {NOW}, 1,
                'agent-writer', 'dev-triage')",
        fx.a.strip_prefix("card:").unwrap(), fx.b.strip_prefix("card:").unwrap(),
    )).unwrap();
    let raw = vec![fx.a.clone()];
    let (req, _) = RecallRequest::handles(&raw).unwrap();
    let o = RecallOpts { include_edges: true, ..Default::default() };
    // 唯一的一条边两格都空 ⇒ 整条不出现 ⇒ 既有形状落 `empty:no_edges`（正常读数，
    // 与故障臂的 `unavailable:store_unreachable` 两码可分——这正是三态可分要的那一格）。
    let err = recall_with(fx.root.memory(), &req, &filter(Some(Viewer::agent("agent-A"))),
        NOW, &o).expect_err("两格都空的边不渲染，显式要边时是正常空读数");
    assert_eq!((err.kind, err.code), (AgentErrorKind::Empty, "no_edges"),
        "合法 JSON 不是故障，分型落在正常空读数那一格：{err:?}");
}
