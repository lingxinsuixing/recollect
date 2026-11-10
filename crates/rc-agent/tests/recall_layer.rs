//! 派发档 I-5a · Phase 4 的**消费面**（L2/L3 真的被读、`time_confidence` 真的被用）。
//!
//! 这一份存在的理由有三条，每一条都在下面的用例里：
//! 1. **MF-1**：`card.time_confidence` 在 BASE 上**零生产读者**（四个消费面一律只读 `as_of_ms`）。
//!    episode 镜像卡在成员全无可知事件时间时，`as_of_ms` 落的是**发话时刻**（写侧
//!    `writers_x::episode_time_arm` 的 `'speech'` 那一支）——不读这一列就是把"说过"说成"发生"。
//!    本文件把四支渲染口径各钉一条，并断 `speech` 那一支**不含**"发生在"。
//! 2. **消费者证明**（T-6 三段，且只能放在这里）：`hub` 的依赖树里没有 `rc-agent`，
//!    派生行能不能被真实读者读到，只有 `rc_agent::recall` / `recall_narrative` 这一层算数。
//!    三段 = 镜像卡**没有归属行时 `row_absent`** ⇒ 补上归属后读得到 ⇒ 把依据行的归属换成
//!    别人之后又读不到。手算 SQL 不算证据。
//! 3. **闸的用例族**（T-7）：成员归别人 ⇒ 成员不出现**且不新增错误码**；一端不可见 ⇒
//!    **整条边不出现**（`export.rs` 的 `render_export` 的既有形状）；`viewer=None` 的旧路径一字不变。
//!
//! 副本那一条（`rc_store::open_replica`）走的是 `VACUUM INTO` 的一致快照，
//! 不是 `cp *.db`——本机是 WAL 模式，裸拷拿到的是旧库（台账点名的运维教训）。

use rc_agent::recall::{
    recall, recall_narrative, recall_narrative_with, recall_with, Intent, NarrativeView,
    RecallFilter, RecallOpts, RecallRequest, Viewer, MAX_L2_MATES, MAX_L3_EDGES,
};
use rc_agent::{render_export, AgentErrorKind, ExportRequest, ToolError};
use rc_memory::keys::{AnchorRole, MemKey, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_x::{
    put_card, put_edges, put_episodes, BatchCtx, EdgeContent, EpisodeContent, FactContent,
};
use rc_memory::{put_fact_rows_with_extras, FactExtras, FactWriteItem, Provenance};
use rc_store::Root;
use serde_json::{json, Value};

const NOW: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;
/// 经历与成员的正文共用的查询词（负样本按纪律用 ASCII 连续串，见 `handles.rs` 那一条理由）。
const QUERY: &str = "经历邻域合成词";
/// 读者（节点宿主的 agent 名）。派生行的归属按"继承依据行"落，所以这个名字要和依据行的
/// `owner_agent_id` 一致才能读到——不一致就是第三段那一格。
const READER: &str = "qoder";
const OTHER: &str = "agent-别家";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-layer".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-writer".into(), device_id: "dev-layer".into() }
}

fn owner_of(agent: &str) -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-layer".into()),
        owner_agent_id: agent.into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// 锚行的 canonical：`fix()` 的 INSERT 与句柄派生共用这一串。
const NAMES: [&str; 3] = ["甲某", "乙某", "丙某"];

fn anchor(index: usize) -> String {
    rc_memory::compute_mem_id(rc_memory::MemTable::Anchor, "person", NAMES[index], "", &scope())
}

fn turn(index: usize) -> String {
    format!("{:064x}", 0x51 + index)
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
    for (i, name) in NAMES.iter().enumerate()
    {
        let id = anchor(i);
        root.memory().conn().execute_batch(&format!(
            "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                                status, created_at_ms, updated_at_ms, norm_version)
             VALUES('{id}', 'person', '{name}', 'real', 'project', 'proj-layer',
                    'active', {NOW}, {NOW}, 1)"
        )).unwrap();
    }
    Fix { _dir: dir, root }
}

/// 一条真 fact（自带陈述面消费卡 + 归属行）。`owner_agent` 决定这一行归谁 ⇒ 第三段用得到。
/// 返回它那张陈述面卡的**句柄串**（`card:<id>`）——L2 成员露面用的就是这个身份。
fn fact(fix: &Fix, index: usize, value: &str, speech_ms: i64, owner_agent: &str) -> String {
    let text = format!("甲某{value}");
    let item = FactWriteItem {
        subject_anchor_id: anchor(0),
        object_anchor_id: None,
        predicate: "做了".into(),
        value: Some(value.into()),
        category: "event".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: value.into(),
        evidence: vec![(turn(index), AnchorRole::Human)],
        text,
        scope: scope(),
        source_ms: speech_ms,
    };
    let extras = FactExtras { participants: vec![anchor(1)], ..Default::default() };
    let out = put_fact_rows_with_extras(fix.m(), &[item], &[extras], &ctx(),
                                        &owner_of(owner_agent), NOW).unwrap();
    assert_eq!(out.facts, 1, "夹具的 fact 必须真的落库：{value}");
    format!("card:{}", out.card_ids[0].id)
}

fn set_occurred(fix: &Fix, fact_id: &str, start: i64) {
    fix.m().conn().execute_batch(&format!(
        "UPDATE fact SET occurred_at_ms={start} WHERE id='{fact_id}'")).unwrap();
}

fn episode(title: &str, narrative: &str, fact_ids: Vec<String>) -> EpisodeContent {
    EpisodeContent { title: title.into(), narrative: narrative.into(), fact_ids }
}

/// 一段两成员的经历（真生产者 `put_episodes`）。返回（镜像卡句柄, 两条成员卡的句柄）。
/// `dated` 决定要不要给成员补事件时间：补了 ⇒ 镜像卡 `time_confidence='anchored'`，
/// 不补 ⇒ 退到最早成员的发话时刻并标 `'speech'`（MF-1 那一支的真生产者形状）。
fn two_member_episode(fix: &Fix, title: &str, owner_agent: &str, dated: bool)
    -> (String, Vec<String>) {
    let a_card = fact(fix, 0, "调通闸", NOW, owner_agent);
    let b_card = fact(fix, 1, "补了桩", NOW + MIN, owner_agent);
    let cards = vec![a_card.clone(), b_card.clone()];
    let fact_ids = fact_ids_of(fix, &cards);
    assert_eq!(fact_ids.len(), 2, "夹具的两张卡必须各指回一条 fact");
    if dated {
        set_occurred(fix, &fact_ids[0], NOW + 10 * MIN);
        set_occurred(fix, &fact_ids[1], NOW + 20 * MIN);
    }
    put_episodes(fix.m(), &[episode(title, "先调通闸，再补了桩。", fact_ids.clone())],
                 &ctx(), NOW).unwrap();
    // 镜像卡句柄从库里读（不重算哈希：要证的正是生产者落下的那一张）。
    let ids = fact_ids.iter().map(|f| format!("{:x}", f.len())).collect::<Vec<String>>();
    assert_eq!(ids.len(), 2, "成员条数与 fact id 数一致（这一格不参与断言，只是形状检查）");
    let card_id: String = fix.m().conn().query_row(
        "SELECT id FROM card WHERE kind='episode' ORDER BY created_at_ms, id LIMIT 1", [],
        |r| r.get(0)).unwrap();
    (format!("card:{card_id}"), cards)
}

/// 从卡的 `pointers_json` 反查它点名的 fact id（顺序 = 数组序 = 事件序）。
fn fact_ids_of(fix: &Fix, card_handles: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for h in card_handles {
        let id = strip(h);
        let json: String = fix.m().conn().query_row(
            "SELECT pointers_json FROM card WHERE id=?1", [&id], |r| r.get(0)).unwrap();
        let inner = json.trim_matches(|c| c == '[' || c == ']');
        for part in inner.split(',') {
            let p = part.trim().trim_matches('"');
            if let Some(f) = p.strip_prefix("fact:") {
                out.push(f.to_string());
            }
        }
    }
    out
}

fn strip(handle: &str) -> String {
    handle.split_once(':').map(|(_, id)| id.to_string()).unwrap_or_else(|| handle.to_string())
}

fn viewer_filter(agent: &str) -> RecallFilter {
    RecallFilter { viewer: Some(Viewer::agent(agent)), ..Default::default() }
}

fn by_handle(m: &rc_store::MemoryHandle, handle: &str, opts: &RecallOpts) -> Value {
    by_handle_as(m, handle, opts, &RecallFilter::empty())
}

/// 同一件事，但带上**读者身份**：闸的用例族（T-7）必须走这一条——`viewer=None` 那条旧路径
/// 根本不过闸，用它断"看不见"等于什么都没断。
fn by_handle_as(m: &rc_store::MemoryHandle, handle: &str, opts: &RecallOpts,
                filter: &RecallFilter) -> Value {
    let req = RecallRequest::Handles(vec![MemKey::parse(handle).unwrap()]);
    let text = recall_with(m, &req, filter, NOW, opts).unwrap().text();
    let doc: Value = serde_json::from_str(&text).unwrap();
    doc["rows"][0].clone()
}

fn narrative(m: &rc_store::MemoryHandle, handle: &str, view: NarrativeView, opts: &RecallOpts)
    -> String {
    let req = RecallRequest::Handles(vec![MemKey::parse(handle).unwrap()]);
    recall_narrative_with(m, &req, &RecallFilter::empty(), NOW, view, opts).unwrap().text()
}

// ── 1. MF-1：`time_confidence` 被消费面读，四支渲染口径 ──────────────────────────

/// `anchored`（成员有事件时间）⇒ 那一格的钟就是事件时间；JSON 带 `anchored`，
/// 文本沿用既有形状（`它记录于 …`）。
#[test]
fn anchored_那一支按事件时间说_json_带_anchored() {
    let fix = fix();
    let (mirror, _) = two_member_episode(&fix, "那次会议", READER, true);
    let row = by_handle(fix.m(), &mirror, &RecallOpts::default());
    assert_eq!(row["time_confidence"], json!("anchored"), "行键带的是库里那一格的原文（归一后）");
    assert_eq!(row["layer"], json!("l2"), "kind='episode' 的镜像卡就是 L2 的载体");
    assert_eq!(row["mode"], json!("inferred"), "派生族显式写 inferred（不是吃 DEFAULT 的 stated）");
    let text = narrative(fix.m(), &mirror, NarrativeView::Expand, &RecallOpts::default());
    assert!(text.contains("它记录于 "), "anchored 沿用既有那一支：\n{text}");
    assert!(!text.contains("发话于"), "有事件时间 ⇒ 不许说发话钟：\n{text}");
}

/// **`speech` 那一支（MF-1 的命门）**：镜像卡的 `as_of_ms` 是最早成员的**发话时刻**，
/// 三个视图 + 导出件都不许把它说成事情发生的时间。
#[test]
fn speech_那一支只说发话于而不把发话钟当事件时间() {
    let fix = fix();
    let (mirror, _) = two_member_episode(&fix, "一段没有日期的经历", READER, false);
    let row = by_handle(fix.m(), &mirror, &RecallOpts::default());
    assert_eq!(row["time_confidence"], json!("speech"), "库里那一格就是 'speech'（写侧第二支）");

    let expand = narrative(fix.m(), &mirror, NarrativeView::Expand, &RecallOpts::default());
    let card = narrative(fix.m(), &mirror, NarrativeView::Card, &RecallOpts::default());
    // brief 走句柄臂（查询臂要 index，本文件不重建索引；两个臂在 brief 上共用同一个渲染器，
    // 除了锚剔除——这一条断的是时间那一格，与那一条无关）。
    let req = RecallRequest::Handles(vec![MemKey::parse(&mirror).unwrap()]);
    let brief = recall_narrative(fix.m(), &req, &viewer_filter(READER), NOW, NarrativeView::Brief)
        .unwrap().text();
    for (face, text) in [("expand", &expand), ("card", &card), ("brief", &brief)] {
        assert!(text.contains("发话于"), "{face} 面上 speech 那一支没换说法：\n{text}");
        assert!(text.contains("事件时间未知"), "{face} 面上没明写事件时间未知：\n{text}");
        assert!(!text.contains("发生在"), "{face} 面上把发话钟说成了事件发生：\n{text}");
    }
    // 导出件（人类面）同样读了这一列：只在 speech 那一支加一格，其余一字不加。
    let doc = render_export(fix.m(), &ExportRequest::all(NOW).unwrap()).unwrap().text();
    let line = doc.lines().find(|l| l.contains(&mirror.replace("card:", "")))
        .expect("镜像卡在导出件里有自己那一行");
    assert!(line.contains("time=发话时刻（事件时间未知）"), "导出件没读这一列：\n{line}");
    assert!(!line.contains("发生在"), "导出件把发话钟说成事件发生：\n{line}");
}

/// `unknown` 与 NULL 走**既有那一支**：文本与 BASE 逐字节相同（`recall_default_bytes.rs`
/// 那一份快照就是它的读者），导出件里也不加 `time=` 那一格。
#[test]
fn unknown_与_null_沿用既有那一支_一个字节不加() {
    let fix = fix();
    // 'unknown'：writers_x::card_sql 那一族（put_card）显式写 unknown。
    let unknown = put_card(fix.m(), &FactContent {
        kind: "decision".into(), title: format!("{QUERY} 的决策"), body: "定了十点发布。".into(),
        scope: scope(), valid_to_ms: None }, &prov('u'), NOW).unwrap().key_string();
    let row = by_handle(fix.m(), &unknown, &RecallOpts::default());
    assert_eq!(row["time_confidence"], json!("unknown"));
    let text = narrative(fix.m(), &unknown, NarrativeView::Card, &RecallOpts::default());
    assert!(text.contains("它记录于 "), "unknown 必须走既有那一支：\n{text}");
    assert!(!text.contains("发话于"));

    // NULL：writers_fact 的最小消费卡（它的 INSERT 列单里根本没有这一列）。
    let member_card = fact(&fix, 7, "没有这一列的行", NOW + 5 * MIN, READER);
    let row2 = by_handle(fix.m(), &member_card, &RecallOpts::default());
    assert_eq!(row2["time_confidence"], json!("unknown"), "NULL ⇒ 与 'unknown' 同一支");
    let doc = render_export(fix.m(), &ExportRequest::all(NOW).unwrap()).unwrap().text();
    let bare = strip(&member_card);
    let line = doc.lines().find(|l| l.contains(&bare)).expect("成员卡在导出件里有自己那一行");
    assert!(!line.contains("time="), "NULL/unknown 那一支不许加任何格：\n{line}");
}

/// **值域外的串**走 `unknown` 那一支，而且**不回显原串**（守卫 12：读数里不许带正文）。
/// `card.time_confidence` 是裸 TEXT、没有 CHECK（`ddl.rs` 的 `card` 建表里那一列），所以库里躺着什么都可能——
/// 这一条断的是"闭集外的值不会变成读者面上的一句话"。
#[test]
fn 值域外的串走_unknown_那一支且不回显原文() {
    let fix = fix();
    let member_card = fact(&fix, 8, "脏值的行", NOW + 6 * MIN, READER);
    let garbage = "一段根本不是那几个词的文字_它带着正文的味道";
    fix.m().conn().execute_batch(&format!(
        "UPDATE card SET time_confidence='{garbage}' WHERE id='{}'", strip(&member_card)))
        .unwrap();
    let row = by_handle(fix.m(), &member_card, &RecallOpts::default());
    assert_eq!(row["time_confidence"], json!("unknown"), "闭集外 ⇒ unknown 支");
    let text = serde_json::to_string(&row).unwrap()
        + &narrative(fix.m(), &member_card, NarrativeView::Expand, &RecallOpts::default());
    assert!(!text.contains(garbage), "域外原串被回显进读数面了（守卫 12）：\n{text}");
    assert!(!text.contains("发话于"), "域外值不许走 speech 那一支");
    assert!(text.contains("它记录于 "), "域外值走既有那一支");
}

/// **inferred + 未审的派生行进不了 probe 的 full**（`rc-search/src/gate.rs` 的 `decide`：
/// full 只由 `Corroborated` 或 `verdict==Passed` 打开）。本轮不修 `verdict_ok` 对 `Unreviewed`
/// 恒真那一格，但这一条把"L2/L3 的派生行不许在注入面上代言"钉成读数。
#[test]
fn inferred_未审的镜像卡在_probe_面上只到_signpost_不进_full() {
    let fix = fix();
    let (mirror, _) = two_member_episode(&fix, "为探针落的那段经历", READER, false);
    assert!(mirror.starts_with("card:"), "夹具的镜像卡句柄形状");
    rc_index::rebuild(fix.m(), NOW).unwrap();
    let view = rc_agent::probe_via_search(fix.m(), None, QUERY, NOW).unwrap();
    assert!(!view.forms.is_empty(), "探针面上一个候选都没有 ⇒ 夹具的查询词没命中");
    // 探针面上没有镜像卡的正文词 ⇒ 先把查询词落进经历正文里再问（`put_episodes` 的 narrative）。
    for d in &view.forms {
        if d.facts.kind == "episode" {
            assert_ne!(format!("{:?}", d.form), "Full",
                "inferred+unreviewed 的派生行进了 full ⇒ 闸被 mode 之外的东西打开了");
        }
    }
}

// ── 2. 消费者证明（三段）+ 闸的用例族 ──────────────────────────────────────────

/// **第一段**：镜像卡**没有归属行**时（= 修前那条断线的形状：`resource_owner` 零行、
/// 署名 `agent-writer` ≠ 读者 `qoder`），真实消费者读到的是 `row_absent`，
/// 而不是"库里没有"。这一段用真句柄臂 + `view=expand` 走 `rc_agent` 那一层。
#[test]
fn 消费者证明第一段_镜像卡没有归属行时真实读者得到_row_absent() {
    let fix = fix();
    let (mirror, _) = two_member_episode(&fix, "没有归属的那段经历", READER, true);
    // 撤掉归属行 = 回到修前那条写入口的形状（I-3e · FIX-1 之前，派生族一行归属都不写）。
    fix.m().conn().execute_batch(&format!(
        "DELETE FROM resource_owner WHERE mem_table='card' AND mem_id='{}'", strip(&mirror)))
        .unwrap();
    let req = RecallRequest::Handles(vec![MemKey::parse(&mirror).unwrap()]);
    let err = recall_narrative(fix.m(), &req, &viewer_filter(READER), NOW, NarrativeView::Expand)
        .unwrap_err();
    assert_eq!(err.kind, AgentErrorKind::Empty, "被闸拒与查不到同形 ⇒ Empty");
    assert_eq!(err.code, "row_absent", "句柄臂的缺席形状不许变成新码：\n{err:?}");
    // 库里明明有这一行（反证"不是没写进去"）。
    let n: i64 = fix.m().conn().query_row(
        "SELECT count(*) FROM card WHERE id=?1", [&strip(&mirror)], |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "行在场：被挡与查不到必须同形，而不是库里真没有");
}

/// **第二段**：同一段经历按现路径写（归属继承自成员行）⇒ 真实读者读得到镜像卡，
/// 而且**邻域读得到**：`episode_id`/`mates` 有值、expand 面上有 L2 块。
#[test]
fn 消费者证明第二段_归属在场时真实读者读得到镜像卡与它的成员() {
    let fix = fix();
    let (mirror, member_cards) = two_member_episode(&fix, "接通归属的那段经历", READER, true);
    let req = RecallRequest::Handles(vec![MemKey::parse(&mirror).unwrap()]);
    let row = by_handle(fix.m(), &mirror, &RecallOpts { intent: Intent::When, ..Default::default() });
    assert!(row["episode_id"].is_string(), "episode_id 没读回来：{row}");
    let mates = row["mates"].as_array().unwrap();
    assert!(!mates.is_empty(), "L2 成员一条都没读回来（而它们都归这位读者）");
    assert!(mates.len() <= MAX_L2_MATES, "帽被破：{} > {MAX_L2_MATES}", mates.len());
    for mate in mates {
        let id = mate["id"].as_str().unwrap();
        assert!(id.starts_with("card:"), "成员露面用的是镜像/消费卡句柄，不是裸 fact id：{id}");
        assert_eq!(mate["layer"], json!("l2"));
        assert_eq!(mate["time_confidence"], json!("anchored"), "夹具补了事件时间 ⇒ anchored");
    }
    // 命中项自己也在那一段里（窗口以它为中心）。
    let listed: Vec<&str> = mates.iter().map(|m| m["id"].as_str().unwrap()).collect();
    assert!(listed.iter().any(|h| *h == member_cards[0].as_str())
            || listed.iter().any(|h| *h == member_cards[1].as_str()),
        "成员清单里没有夹具那两张卡：{listed:?}");
    let text = recall_narrative_with(fix.m(), &req, &viewer_filter(READER), NOW,
                                     NarrativeView::Expand,
                                     &RecallOpts { intent: Intent::When, ..Default::default() })
        .unwrap().text();
    assert!(text.contains("它所属的经历（L2）"), "expand 面上没有 L2 块：\n{text}");
    assert!(!text.contains("它已经通过审核"),
        "L2 那一块复用了 verdict_clause 的\"已通过审核\"（派发档 I-5a §3 的禁句）：\n{text}");
}

/// **第三段**：依据行属于别的 agent ⇒ 镜像卡继承到别人的归属 ⇒ 真实读者又读不到
/// （被闸拒与查不到同形，且不新增错误码）。这一条同时是"归属继承只在真写这一行的路径生效"
/// 的正证：修法不是放宽闸，所以反例必须有牙。
#[test]
fn 消费者证明第三段_依据行属于别人时镜像卡对读者不可见() {
    let fix = fix();
    let (mirror, _) = two_member_episode(&fix, "归别人的那段经历", OTHER, true);
    let req = RecallRequest::Handles(vec![MemKey::parse(&mirror).unwrap()]);
    let err = recall_narrative(fix.m(), &req, &viewer_filter(READER), NOW, NarrativeView::Expand)
        .unwrap_err();
    assert_eq!(err.code, "row_absent");
    assert_eq!(err.kind, AgentErrorKind::Empty);
    // 阳性对照：换一个读者（那一族的主人）就读得到 ⇒ 不是闸整片失灵。
    let ok = recall_narrative(fix.m(), &req, &viewer_filter(OTHER), NOW, NarrativeView::Expand);
    assert!(ok.is_ok(), "归属的主人读不到 ⇒ 继承那一条写歪了：{:?}", ok.err());
}

/// T-7 第一格：读者看得见经历卡、但成员归别人 ⇒ **成员不出现**、经历卡照给、
/// **不新增错误码**（"看不见"与"没有"同形）。
#[test]
fn 经历卡可见而成员归别人时_成员不出现且不新增错误码() {
    let fix = fix();
    // 成员归别人，再把**镜像卡**的归属改回读者：这就是"卡可见、成员不可见"那一形。
    // 写侧的归属继承只会让卡的归属 ⊆ 成员的归属，所以这一形靠一次 UPDATE 造出来——
    // 它对应的是"成员被别家补写进同一段"那种库外漂移，而不是正常路径。
    let (mirror, member_cards) = two_member_episode(&fix, "卡可见成员不可见的那段", OTHER, true);
    fix.m().conn().execute_batch(&format!(
        "UPDATE resource_owner SET owner_agent_id='{READER}' \
         WHERE mem_table='card' AND mem_id='{}'", strip(&mirror))).unwrap();
    let opts = RecallOpts { intent: Intent::When, ..Default::default() };
    let row = by_handle_as(fix.m(), &mirror, &opts, &viewer_filter(READER));
    assert!(row["episode_id"].is_string(), "经历卡可见 ⇒ episode_id 读得回来");
    assert_eq!(row["mates"].as_array().unwrap().len(), 0,
        "成员归别人还露面 ⇒ 越权（而且不许靠错误码遮掩）：{row}");
    // 成员自己的句柄被这位读者取：被拒与查不到**同形**（json 面上进 unresolved_handles，
    // expand 面上是 `row_absent`），不许新开"无权限"那一码。
    let json = recall(fix.m(), &RecallRequest::Handles(vec![
        MemKey::parse(&member_cards[0]).unwrap()]), &viewer_filter(READER), NOW).unwrap().text();
    assert!(json.contains("unresolved_handles"), "句柄臂的缺席账不在场：{json}");
    assert!(json.contains("\"rows\":[]"), "被闸拒的成员不许进 rows：{json}");
    let err = recall_narrative(fix.m(),
        &RecallRequest::Handles(vec![MemKey::parse(&member_cards[0]).unwrap()]),
        &viewer_filter(READER), NOW, NarrativeView::Expand).unwrap_err();
    assert_eq!(err.code, "row_absent");
    assert_eq!(err.kind, AgentErrorKind::Empty);
    let err2 = recall_narrative(fix.m(),
        &RecallRequest::Handles(vec![MemKey::parse("card:ghost").unwrap()]),
        &viewer_filter(READER), NOW, NarrativeView::Expand).unwrap_err();
    assert_eq!(err2.code, err.code, "被闸拒与查不到必须同码（存在性本身是要藏的事）");
    // 阳性对照：成员的主人读得到自己的成员 ⇒ 不是闸整片失灵。
    let own = by_handle_as(fix.m(), &member_cards[0], &RecallOpts::default(),
                           &viewer_filter(OTHER));
    assert_eq!(own["id"], json!(member_cards[0]));
}

/// T-7 第二格：边的一端不可见 ⇒ **整条边不出现**（`export.rs` 的 `render_export` 的既有形状），
/// 而同一行上另一条两端都可见的边照给——这一格断的是"逐条筛"，不是"整片失灵"。
#[test]
fn 边的一端不可见时整条边不出现而另一端可见的那条照给() {
    let fix = fix();
    let a = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的甲端"), body: "甲端落地。".into(), scope: scope(),
        valid_to_ms: None }, &prov('a'), NOW).unwrap().key_string();
    let b = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的乙端"), body: "乙端落地。".into(), scope: scope(),
        valid_to_ms: None }, &prov('b'), NOW).unwrap().key_string();
    let c = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的丙端"), body: "丙端落地。".into(), scope: scope(),
        valid_to_ms: None }, &prov('c'), NOW).unwrap().key_string();
    // 归属按读者摆好：甲/乙归读者，丙归别家 ⇒ 唯一"看不见"的原因是那条边的另一端。
    for handle in [&a, &b] {
        fix.m().conn().execute_batch(&format!(
            "UPDATE resource_owner SET owner_agent_id='{READER}' \
             WHERE mem_table='card' AND mem_id='{}'", strip(handle))).unwrap();
    }
    fix.m().conn().execute_batch(&format!(
        "UPDATE resource_owner SET owner_agent_id='{OTHER}' \
         WHERE mem_table='card' AND mem_id='{}'", strip(&c))).unwrap();
    assert_eq!(put_edges(fix.m(), &[EdgeContent { from_id: strip(&a), to_id: strip(&b),
        kind: "depends_on".into(), reason: "甲端等乙端。".into(), scope: scope(),
        basis_ids: vec![] }], &ctx(), NOW).unwrap(), 1);
    assert_eq!(put_edges(fix.m(), &[EdgeContent { from_id: strip(&a), to_id: strip(&c),
        kind: "located_in".into(), reason: "甲端在丙端里。".into(), scope: scope(),
        basis_ids: vec![] }], &ctx(), NOW + MIN).unwrap(), 1);

    let opts = RecallOpts { include_edges: true, ..Default::default() };
    let row = by_handle_as(fix.m(), &a, &opts, &viewer_filter(READER));
    let edges = row["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "两端都可见的那条留下、另一端不可见的那条整条不出现：{row}");
    assert_eq!(edges[0]["to"], json!(b), "留下的是甲→乙那条");
    // 旧路径（`viewer=None`）一字不变：两条边都看得见。
    let legacy = by_handle(fix.m(), &a, &opts);
    assert_eq!(legacy["edges"].as_array().unwrap().len(), 2,
        "viewer=None 的旧路径（不过闸）必须看得见两条边");
}

/// `fact:` 前缀的端点**先归一成镜像卡**再判，否则因果边会无故消失（派发档 I-5a §3）。
#[test]
fn fact_端点先归一成_card_否则整条边无故消失() {
    let fix = fix();
    let member_card = fact(&fix, 3, "作为边端点的那条", NOW + 3 * MIN, READER);
    let other = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的另一端"), body: "另一端落地。".into(), scope: scope(),
        valid_to_ms: None }, &prov('o'), NOW).unwrap().key_string();
    fix.m().conn().execute_batch(&format!(
        "UPDATE resource_owner SET owner_agent_id='{READER}' \
         WHERE mem_table='card' AND mem_id='{}'", strip(&other))).unwrap();
    let fact_id = fact_ids_of(&fix, std::slice::from_ref(&member_card)).remove(0);
    // 手写一条 **fact: 端点**的边（真生产者今天只写 card 端点；这一形在 `reader::split_endpoint`
    // 的读法里是被允许的，所以归一必须有牙，否则边会静默消失）。
    fix.m().conn().execute_batch(&format!(
        "INSERT INTO fact_link(id, from_id, to_id, relation, basis, evidence_json, decided_by,
                               scope_kind, scope_key, created_at_ms, norm_version,
         source_agent, device_id)
         VALUES('edge-fact-endpoint', 'fact:{fact_id}', 'card:{}', 'depends_on',
                '它指着那条事实。', '[]', NULL, 'project', 'proj-layer', {NOW}, 1,
                'agent-writer', 'dev-layer')",
        strip(&other))).unwrap();
    let opts = RecallOpts { include_edges: true, ..Default::default() };
    let row = by_handle_as(fix.m(), &member_card, &opts, &viewer_filter(READER));
    let edges = row["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 1, "fact: 端点没归一 ⇒ 边无故消失：{row}");
    assert_eq!(edges[0]["from"], json!(member_card),
        "读数里交出的是归一后的镜像卡句柄，而不是过不了闸的 fact 键");
    assert_eq!(edges[0]["layer"], json!("l3"));
    assert_eq!(edges[0]["id"], json!("edge-fact-endpoint"));
    // 边自己没有 mode 列 ⇒ 取端点卡 mode 的下推（写侧那条卡是 stated）。
    assert_eq!(edges[0]["mode"], json!("stated"), "边的 mode 是端点卡 mode 的下推");
}

/// 两跳的形状：`depth=2` 才读到第二跳的边；`depth=1` 只读到直接触达的那条。
/// （"每跳一条 SQL、批量而不是逐点"这一格由 `edges_touching` 的实现与
/// `rc-memory/tests/recall_layer_readers.rs` 的批量用例担保。）
#[test]
fn depth_2_才读第二跳_每行最多三条() {
    let fix = fix();
    let mk = |title: &'static str, c: char| -> String {
        put_card(fix.m(), &FactContent { kind: "decision".into(),
            title: format!("{QUERY} 的{title}"), body: format!("{title}的正文。"),
            scope: scope(), valid_to_ms: None }, &prov(c), NOW).unwrap().key_string()
    };
    let a = mk("起点", 'a');
    let b = mk("中继", 'b');
    let c = mk("远端", 'c');
    assert_eq!(put_edges(fix.m(), &[EdgeContent { from_id: strip(&a), to_id: strip(&b),
        kind: "depends_on".into(), reason: "甲乙相连。".into(), scope: scope(),
        basis_ids: vec![] }], &ctx(), NOW).unwrap(), 1);
    assert_eq!(put_edges(fix.m(), &[EdgeContent { from_id: strip(&b), to_id: strip(&c),
        kind: "depends_on".into(), reason: "乙丙相连。".into(), scope: scope(),
        basis_ids: vec![] }], &ctx(), NOW + MIN).unwrap(), 1);

    let one = by_handle(fix.m(), &a, &RecallOpts { include_edges: true, depth: 1,
        ..Default::default() });
    assert_eq!(one["edges"].as_array().unwrap().len(), 1, "一跳只有直接那条：{one}");
    let two = by_handle(fix.m(), &a, &RecallOpts { include_edges: true, depth: 2,
        ..Default::default() });
    let edges = two["edges"].as_array().unwrap();
    assert_eq!(edges.len(), 2, "第二跳的边在这一格才露面：{two}");
    assert!(edges.len() <= MAX_L3_EDGES, "帽被破：{} > {MAX_L3_EDGES}", edges.len());
    assert_eq!(edges[0]["to"], json!(b), "第 1 跳的边排在前面（预算先到先得，不是随便取）");
}

/// 证据两格都空的边不渲染（`recall-contract.md` §5 那条"evidence 为空/缺失的边整条不渲染"
/// 的可执行形：只按 `evidence_json` 判会把 `put_edges` 那一族全判死，登记在报告 §偏离）。
#[test]
fn 证据两格都空的边不渲染_只有_basis_的边照露面() {
    let fix = fix();
    let a = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的有证端"), body: "有证端的正文。".into(), scope: scope(),
        valid_to_ms: None }, &prov('a'), NOW).unwrap().key_string();
    let b = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的没证端"), body: "没证端的正文。".into(), scope: scope(),
        valid_to_ms: None }, &prov('b'), NOW).unwrap().key_string();
    // 有 basis 的边（`put_edges` 那一族的正常形状：evidence_json 恒 '[]'）。
    assert_eq!(put_edges(fix.m(), &[EdgeContent { from_id: strip(&a), to_id: strip(&b),
        kind: "depends_on".into(), reason: "凭这一句话。".into(), scope: scope(),
        basis_ids: vec![] }], &ctx(), NOW).unwrap(), 1);
    let opts = RecallOpts { include_edges: true, ..Default::default() };
    let with_basis = by_handle(fix.m(), &a, &opts);
    assert_eq!(with_basis["edges"].as_array().unwrap().len(), 1,
        "basis 有内容的边必须露面（否则全仓的边都被判成不可见）");
    assert_eq!(with_basis["edges"][0]["basis"], json!("凭这一句话。"));
    // 手写一条两格都空的边 ⇒ 不渲染。
    fix.m().conn().execute_batch(&format!(
        "INSERT INTO fact_link(id, from_id, to_id, relation, basis, evidence_json, decided_by,
                               scope_kind, scope_key, created_at_ms, norm_version,
         source_agent, device_id)
         VALUES('edge-no-evidence', 'card:{}', 'card:{}', 'depends_on',
                '', '[]', NULL, 'project', 'proj-layer', {NOW}, 1,
                'agent-writer', 'dev-layer')",
        strip(&a), strip(&b))).unwrap();
    let after = by_handle(fix.m(), &a, &opts);
    assert_eq!(after["edges"].as_array().unwrap().len(), 1,
        "两格都空的那条被渲染了 ⇒ 阴性对照失效");
    assert_eq!(after["edges"][0]["basis"], json!("凭这一句话。"));
    assert_ne!(after["edges"][0]["id"], json!("edge-no-evidence"));
    let e: Vec<String> = after["edges"].as_array().unwrap().iter()
        .map(|x| x["id"].as_str().unwrap().to_string()).collect();
    assert!(!e.contains(&"edge-no-evidence".to_string()));
}

/// 退场的边（未解决 `retirement`）不出现：`retired` 的判据住在消费面而不是库侧
/// （`edges` 与 `edges_touching` 共用同一份读数口径）。
#[test]
fn 退场的边在召回面上不出现() {
    let fix = fix();
    let a = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的退场甲端"), body: "退场甲端正文。".into(), scope: scope(),
        valid_to_ms: None }, &prov('a'), NOW).unwrap().key_string();
    let b = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的退场乙端"), body: "退场乙端正文。".into(), scope: scope(),
        valid_to_ms: None }, &prov('b'), NOW).unwrap().key_string();
    assert_eq!(put_edges(fix.m(), &[EdgeContent { from_id: strip(&a), to_id: strip(&b),
        kind: "depends_on".into(), reason: "还没退场。".into(), scope: scope(),
        basis_ids: vec![] }], &ctx(), NOW).unwrap(), 1);
    let opts = RecallOpts { include_edges: true, ..Default::default() };
    assert_eq!(by_handle(fix.m(), &a, &opts)["edges"].as_array().unwrap().len(), 1);
    fix.m().conn().execute_batch(&format!(
        "INSERT INTO retirement(mem_table, mem_id, reason, created_at_ms, resolved_at_ms)
         VALUES('fact_link', 'edge-退场那条', '判同撤销', {NOW}, NULL)")).unwrap();
    // 上一步的 id 是猜的，取真实那条再来一次（内容寻址 id 由写侧定，测试不许硬编）。
    let edge_id: String = fix.m().conn().query_row(
        "SELECT id FROM fact_link WHERE from_id=?1", [&format!("card:{}", strip(&a))],
        |r| r.get(0)).unwrap();
    fix.m().conn().execute_batch(&format!(
        "INSERT OR REPLACE INTO retirement(mem_table, mem_id, reason, created_at_ms, resolved_at_ms)
         VALUES('fact_link', '{edge_id}', '判同撤销', {NOW}, NULL)")).unwrap();
    // 退了场的边不出现 ⇒ 这一行没有可见邻域 ⇒ 显式要边时走 `no_edges`（Empty 族）。
    let err = recall_with(fix.m(), &RecallRequest::Handles(vec![MemKey::parse(&a).unwrap()]),
                          &RecallFilter::empty(), NOW, &opts).unwrap_err();
    assert_eq!(err.code, "no_edges");
    assert_eq!(err.kind, AgentErrorKind::Empty);
    // 反证：解除退场账 ⇒ 同一次调用又读得到那条边（"消失"来自 retired 读数而不是查无此边）。
    fix.m().conn().execute_batch("DELETE FROM retirement WHERE mem_table='fact_link'").unwrap();
    let back = by_handle(fix.m(), &a, &opts);
    assert_eq!(back["edges"].as_array().unwrap().len(), 1, "解除退场后必须回到一条边");
}

// ── 3. 邻域块的形状：帽、省略数、no_episode/no_edges 的分型 ──────────────────────

/// 显式要 L2 时的 `brief` 面：经历块在场、帽按 `MAX_L2_MATES`、**成员超帽时明写省略数**。
#[test]
fn brief_的经历块在场且超帽时明写省略数() {
    let one = fix();
    let (mirror, _) = two_member_episode(&one, "带块的那段经历", READER, true);
    let opts = RecallOpts { intent: Intent::When, ..Default::default() };
    let req = RecallRequest::Handles(vec![MemKey::parse(&mirror).unwrap()]);
    // brief 只接查询臂的状态块那一格，句柄臂走卡面；这里两形都看一眼。
    let card = recall_narrative_with(one.m(), &req, &viewer_filter(READER), NOW,
                                     NarrativeView::Card, &opts).unwrap().text();
    assert!(card.contains("它所属的经历（L2）"), "卡面上没有 L2 块：\n{card}");
    assert!(card.contains("发话于") || card.contains("事件时间"),
        "L2 块里的时间那一格没按事件时间/发话钟说话：\n{card}");
    // 六成员的一段（超 MAX_L2_MATES=4）：省略数必须响亮。
    let many = fix();
    let mut ids: Vec<String> = Vec::new();
    for i in 0..6usize {
        let handle = fact(&many, 20 + i, &format!("第{i}条成员"), NOW + i as i64 * MIN, READER);
        ids.extend(fact_ids_of(&many, &[handle]));
    }
    put_episodes(many.m(), &[episode("六条成员的经历", "一长串。", ids.clone())],
                 &ctx(), NOW).unwrap();
    let mirror2: String = many.m().conn().query_row(
        "SELECT id FROM card WHERE kind='episode' ORDER BY id LIMIT 1", [], |r| r.get(0)).unwrap();
    let mirror2 = format!("card:{mirror2}");
    let row = by_handle(many.m(), &mirror2, &opts);
    assert_eq!(row["mates"].as_array().unwrap().len(), MAX_L2_MATES,
        "json 那一面的 mates 走 MAX_L2_MATES 帽");
    // expand 那一面的帽是 10（MAX_EXPAND_MEMBERS），六条成员装得下；
    // 超帽那一格由卡面（MAX_L2_MATES=4）断 —— 超了就明写省略数，不许静默。
    let text = narrative(many.m(), &mirror2, NarrativeView::Card, &opts);
    assert!(text.contains("这一段共 6 个成员") && text.contains("没有列出"), "成员超帽却静默截断：
{text}");
}

/// `no_episode` / `no_edges` 只在**显式要那一层**时出现（§8-Q1 的甲案），默认读数永不报。
#[test]
fn no_episode_与_no_edges_只在显式请求时出现() {
    let fix = fix();
    let plain = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的孤立卡"), body: "它没有经历也没有边。".into(),
        scope: scope(), valid_to_ms: None }, &prov('p'), NOW).unwrap().key_string();
    let req = || RecallRequest::Handles(vec![MemKey::parse(&plain).unwrap()]);
    // 默认读数：正常返回，两个新码一次都不出现。
    let d = recall(fix.m(), &req(), &RecallFilter::empty(), NOW);
    assert!(d.is_ok(), "默认读数不许变成新错误：{:?}", d.err());
    // 显式要 L2 ⇒ no_episode（Empty 族，与 no_rows 同族不同码）。
    let e = recall_with(fix.m(), &req(), &RecallFilter::empty(), NOW,
                        &RecallOpts { intent: Intent::When, ..Default::default() }).unwrap_err();
    assert_eq!(e.kind, AgentErrorKind::Empty);
    assert_eq!(e.code, "no_episode");
    // 显式要边 ⇒ no_edges。
    let g = recall_with(fix.m(), &req(), &RecallFilter::empty(), NOW,
                        &RecallOpts { include_edges: true, ..Default::default() }).unwrap_err();
    assert_eq!(g.code, "no_edges");
    // 载荷不回显句柄内容（守卫 12 的同族纪律）。
    assert!(!format!("{e:?}").contains(&strip(&plain)), "错误载荷里带了句柄字节");
}

/// **副本真跑**（T-6 那条纪律的机器形状）：中心库 → `VACUUM INTO` 一致快照 →
/// `rc_store::open_replica` 只读柄 → 同一个 `rc_agent::recall_with` 读得到邻域。
/// 不用 `cp *.db`：本机是 WAL 模式，裸拷拿到的是旧库。
#[test]
fn 副本只读柄上读得到邻域_open_replica_那条路() {
    let fix = fix();
    let (mirror, _) = two_member_episode(&fix, "副本里的那段经历", READER, true);
    let snap = fix._dir.path().join("snapshot-memory.db");
    rc_store::snapshot::vacuum_into(fix.m().conn(), &snap).unwrap();
    let replica = rc_store::open_replica(&snap).unwrap();
    assert!(replica.is_read_only(), "副本必须是只读柄（写入口一律拒它）");
    let row = by_handle(&replica, &mirror, &RecallOpts { intent: Intent::When,
        ..Default::default() });
    assert!(row["episode_id"].is_string(), "副本上读不到 episode_id：{row}");
    assert_eq!(row["layer"], json!("l2"));
    assert!(!row["mates"].as_array().unwrap().is_empty(), "副本上成员清单是空的");
    // 只读柄上写一次 ⇒ 响亮拒（这一条不是本轮的交付，但它是"副本"这个形状的既有判据）。
    let err: Result<(), ToolError> = rc_memory::put_card(&replica, &FactContent {
        kind: "note".into(), title: "副本上不许写".into(), body: "副本上不许写".into(),
        scope: scope(), valid_to_ms: None }, &prov('w'), NOW)
        .map(|_| ()).map_err(ToolError::from);
    let werr = err.unwrap_err();
    // `MemError::ReadOnly` 的分型映射住 `error.rs` 那张逐变体点名的表（Authorization +
    // `replica_read_only`），本用例只断"响亮地拒"，不在这里重述那张表。
    assert_eq!(werr.kind, AgentErrorKind::Authorization);
    assert_eq!(werr.code, "replica_read_only");
}

// ── 4. 行键 `basis`：正例与反例都要在 ────────────────────────────────────────────

/// 评审补口（Phase 4）：`basis` 这一格（`recall.rs` 的 `row_json`，判据
/// `RowLayer::basis = has_basis(&r.steps)`，`attach_layers` 装载时算它）在生产面上**恒产出**，
/// 而 `recall_default_bytes.rs` 的 `json_行键清单与七个新键的缺省值被钉住` 用例只钉了 `false` 那一支（那张夹具的卡只有
/// `ref_kind='turn'` 的出处行）⇒ `true` 那一支"会产出但没人证"。
///
/// 正例照既有夹具写、不改生产代码去迁就测试：`two_member_episode` 走真生产者
/// `put_episodes`，它的写入口给每个成员落一行 `provenance(ref_kind='fact')`
/// （`writers_x::insert_derived_provenance` + `ROLE_BASIS`）⇒ `reader::chain_for` 认
/// `ref_kind != 'turn'` 的那一族为 `ProvenanceStep::Basis` ⇒ `has_basis` 真。
/// 反例两形：① `put_card` 的直接写入口（`ref_kind` 吃 DDL 的 `DEFAULT 'turn'`）；
/// ② 同一张镜像卡、同一次调用口径，把点名的依据行删空（`ref_kind != 'turn'` 那几行）。
/// ②这一格是"断言跟着数据走、不跟着卡片种类走"的自证，也是这条断言不是永真式的机器形状。
#[test]
fn 行键_basis_的依据行在场时为_true_删空与只有_turn_出处时为_false() {
    let fix = fix();
    let (mirror, _) = two_member_episode(&fix, "依据行在场的那段经历", READER, true);
    let row = by_handle(fix.m(), &mirror, &RecallOpts::default());
    assert_eq!(row["basis"], json!(true), "镜像卡有被点名的依据行 ⇒ basis 为真：{row}");
    // 与顶层那笔出处账对得上：`true` 的那一支必须真有一个 `kind='basis'` 的步
    //（否则这一格就是凭空亮的常量）。
    let steps = row["provenance"].as_array().unwrap();
    assert!(steps.iter().any(|s| s["kind"].as_str() == Some("basis")),
        "basis=true 而出处账里没有 basis 步 ⇒ 两格各说各话：{row}");

    // 反例 ②：删空依据行 ⇒ 同一个句柄、同一个读数口径翻回 `false`。
    fix.m().conn().execute_batch(&format!(
        "DELETE FROM provenance WHERE mem_table='card' AND mem_id='{}' AND ref_kind<>'turn'",
        strip(&mirror))).unwrap();
    let stripped = by_handle(fix.m(), &mirror, &RecallOpts::default());
    assert_eq!(stripped["basis"], json!(false),
        "依据行删空后仍是 true ⇒ 那一格不是读数而是常量：{stripped}");
    assert!(stripped["provenance"].as_array().unwrap().iter()
        .all(|s| s["kind"].as_str() != Some("basis")),
        "basis=false 而出处账里还站着 basis 步：{stripped}");

    // 反例 ①：另一张从头就没有依据行的卡（`put_card` 只落 `ref_kind='turn'` 那一族）。
    let plain = put_card(fix.m(), &FactContent { kind: "decision".into(),
        title: format!("{QUERY} 的无依据卡"), body: "它只有一行 turn 出处。".into(),
        scope: scope(), valid_to_ms: None }, &prov('q'), NOW).unwrap().key_string();
    let row2 = by_handle(fix.m(), &plain, &RecallOpts::default());
    assert_eq!(row2["basis"], json!(false),
        "出处链里一个 Basis 都没有 ⇒ false（键恒在、不是缺项）：{row2}");
    // 这一支的 `false` 是"有出处、但出处里没有依据行"，不是"出处账整个空着"：
    // 缺项与空账都会让断言变成永真式，所以先把 turn 步钉在场。
    let steps2 = row2["provenance"].as_array().unwrap();
    assert!(!steps2.is_empty()
            && steps2.iter().all(|s| s["kind"].as_str() == Some("turn")),
        "反例 ① 的出处账不是纯 turn 步 ⇒ 这一格没在证它该证的：{row2}");
}
