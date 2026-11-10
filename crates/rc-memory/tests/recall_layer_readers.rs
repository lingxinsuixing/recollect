//! 派发档 I-5a · Phase 4 的**读原语**（库侧）。
//!
//! 这一层只断"读得到、读的是哪一格、按什么序"，**不**断渲染与权限（那是
//! `crates/rc-agent/tests/recall_layer.rs` 那一族的职责）。五条原语逐字照
//! `investigations/recall-contract.md` §5.2 与派发档 I-5a §2 的清单，签名不许自创第二套：
//!
//! | 原语 | 它替谁补位 |
//! |---|---|
//! | [`reader::card_for_fact`] | fact 行**没有 `MemKey`**（`MemTable` 只有 `Anchor|Card`），
//!   L2 成员要过闸就只有这一条能过的句柄 |
//! | [`reader::episode_card_for_fact`] | 同一条 fact 的**经历镜像卡**（`kind='episode'`）——
//!   与陈述面卡两族，混用会把"这段经历被下一段顶掉"读成"这条事实被判死" |
//! | [`reader::card_fact_pointers`] | L1 命中行 → 它的 fact → 它的 episode 那条链的第一跳
//!   （此前两个指针读者都把解法埋在函数内部，召回面拿不到） |
//! | [`reader::edges_touching`] | L3 边：一条 SQL 覆盖 from/to 两臂；深 2 是**再批量一次**而不是逐点 |
//! | [`reader::card_modes`] | `mode` + `time_confidence` 的**侧查**位（`RowFacts` 一个字段都不许加：
//!   全仓 14 处结构体字面量，含 `recall.rs::filter_tests::base()` 那处不带 `..Default` 的） |
//!
//! 夹具一律走真生产者（`put_fact_rows_with_extras` / `put_episodes` / `put_edges` /
//! `put_timeline_rows_counted`），不在测试里重算内容寻址句柄以外的东西；手搭的 SQL 只用于
//! "真生产者产不出的那一形"（没有消费卡的 fact），并且写明为什么。

use rc_memory::identity::{compute_episode_card_id, compute_fact_id};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::reader::{self, CardMode, EdgeFacts};
use rc_memory::writers_x::{put_card, put_edges, put_mentions, put_episodes,
                           put_timeline_rows_counted, BatchCtx, EdgeContent, EpisodeContent,
                           FactContent, MentionContent, TimelineContent};
use rc_memory::{put_fact_rows_with_extras, retire_event_link, AnchorRole, FactExtras,
                FactWriteItem, MemKey, MemTable, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

/// 执行钟。episode 那一族**不许**用它写任何一格（见 `writers_x::episode_time_arm`），
/// 所以它与源时刻故意差得很远。
const NOW: i64 = 1_700_000_999_000;
/// 发话钟基准（`fact.created_at_ms` 由它来）。
const SRC0: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-readers".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-readers".into(), device_id: "dev-readers-1".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-readers".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// 锚行的 canonical：`seed_anchor` 的 INSERT 与句柄派生共用这一串。
const NAMES: [&str; 3] = ["甲某", "乙某", "丙某"];

fn anchor(index: usize) -> String {
    rc_memory::compute_mem_id(MemTable::Anchor, "person", NAMES[index], "", &scope())
}

fn turn(index: usize) -> String {
    format!("{:064x}", 0x41 + index)
}

fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

fn seed_anchor(m: &MemoryHandle, id: &str, canonical: &str) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, scope_kind, scope_key,
                            status, created_at_ms, updated_at_ms, norm_version)
         VALUES(?1, 'person', ?2, 'real', 'project', ?3, 'active', ?4, ?4, 1)",
        rusqlite::params![id, canonical, "proj-readers", SRC0],
    ).unwrap();
}

fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for (index, name) in NAMES.iter().enumerate() {
        seed_anchor(root.memory(), &anchor(index), name);
    }
    (dir, root)
}

/// 一条真 fact 行 + 它那张**陈述面消费卡**（`writers_fact` 每落一条 fact 配一张指回它的卡，
/// 归属两行也随行落库）。id 从真生产者的账本拿，不在夹具里重算哈希。
fn fact_and_card(m: &MemoryHandle, index: usize, value: &str, speech_ms: i64) -> (String, String) {
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
        text: text.clone(),
        scope: scope(),
        source_ms: speech_ms,
    };
    let id = compute_fact_id(&anchor(0), "做了", None, Some(value), "event", "", &text, &scope());
    let extras = FactExtras { participants: vec![anchor(1)], ..Default::default() };
    let out = put_fact_rows_with_extras(m, &[item], &[extras], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 1, "夹具的 fact 必须真的落库：{value}");
    assert_eq!(out.card_ids.len(), 1, "夹具的 fact 必须自带一张陈述面卡：{value}");
    (id, out.card_ids[0].id.clone())
}

fn set_occurred(m: &MemoryHandle, id: &str, start: Option<i64>) {
    m.conn().execute(
        "UPDATE fact SET occurred_at_ms=?1 WHERE id=?2",
        rusqlite::params![start, id],
    ).unwrap();
}

fn episode(title: &str, narrative: &str, fact_ids: Vec<String>) -> EpisodeContent {
    EpisodeContent { title: title.into(), narrative: narrative.into(), fact_ids }
}

fn card(m: &MemoryHandle, title: &str, body: &str, c: char) -> String {
    put_card(m, &FactContent { kind: "decision".into(), title: title.into(), body: body.into(),
                               scope: scope(), valid_to_ms: None }, &prov(c), NOW).unwrap().id
}

fn edge(from: &str, to: &str, kind: &str, reason: &str, basis_ids: Vec<String>) -> EdgeContent {
    EdgeContent { from_id: from.into(), to_id: to.into(), kind: kind.into(),
                  reason: reason.into(), scope: scope(), basis_ids }
}

fn ids_of(rows: &[EdgeFacts]) -> Vec<String> {
    rows.iter().map(|e| e.id.clone()).collect()
}

// ── card_for_fact / episode_card_for_fact / card_fact_pointers ──────────────────

/// L2 的命门形状：同一条 fact 被**两族**卡指回（陈述面卡 `mode='stated'`、
/// 经历镜像卡 `mode='inferred'` + `kind='episode'`），两条 reader 各认各的一族。
/// 混用就是写侧 `member_cards_are_silenced` 那条实测警告的读侧镜像。
#[test]
fn 同一条_fact_的两族卡各归各的_reader() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let (fa, stated) = fact_and_card(m, 0, "开会", SRC0);
    let (fb, _) = fact_and_card(m, 1, "散会", SRC0 + MIN);
    set_occurred(m, &fa, Some(SRC0 + 10 * MIN));
    put_episodes(m, &[episode("那次会议", "开了会并散了会。", vec![fa.clone(), fb.clone()])],
                 &ctx(), NOW).unwrap();
    let mirror = compute_episode_card_id(&scope(), &[fa.clone(), fb.clone()]);

    let got = reader::card_for_fact(m, &fa).unwrap().expect("陈述面卡是生产者写下的");
    assert_eq!(got.table, MemTable::Card, "过闸句柄只能是 card（MemTable 没有 fact 变体）");
    assert_eq!(got.id, stated, "card_for_fact 取陈述面卡，不取经历镜像卡");
    let got_mirror = reader::episode_card_for_fact(m, &fa).unwrap()
        .expect("镜像卡也在库里");
    assert_eq!(got_mirror.id, mirror, "episode_card_for_fact 取那一段经历的卡");
    assert_ne!(got.id, got_mirror.id, "两族句柄必须是两个不同的东西（同一个 fact、两件事）");
    // 镜像卡的指针是**全体成员**、陈述面卡的指针只有它那一条（这一格决定 mates 从哪读）。
    assert_eq!(reader::card_fact_pointers(m, &stated).unwrap(), vec![fa.clone()]);
    assert_eq!(reader::card_fact_pointers(m, &mirror).unwrap(), vec![fa.clone(), fb.clone()]);
}

/// 认不出的 fact（库里根本没有这一行）与"有 fact、没有陈述面卡"两形都必须是 `None`，
/// 而不是替读者编一个句柄。后一形用**手搭** SQL：真生产者永远成对写，只有库外删除能造出它。
#[test]
fn card_for_fact_没有消费卡时返回_none_而不是编一个句柄() {
    let (_dir, root) = seeded();
    let m = root.memory();
    assert!(reader::card_for_fact(m, &"f".repeat(64)).unwrap().is_none(),
        "根本不存在的 fact 不许命中任何卡");
    assert!(reader::episode_card_for_fact(m, &"f".repeat(64)).unwrap().is_none());

    let (fa, stated) = fact_and_card(m, 0, "唯一一条", SRC0);
    assert_eq!(reader::card_for_fact(m, &fa).unwrap().unwrap().id, stated);
    m.conn().execute("DELETE FROM card WHERE id=?1", [&stated]).unwrap();
    assert!(reader::card_for_fact(m, &fa).unwrap().is_none(),
        "卡被删掉 ⇒ 没有能过闸的句柄；这里不许退化成 episode 卡或裸 fact id");
    assert!(reader::card_fact_pointers(m, &"c".repeat(64)).unwrap().is_empty(),
        "不存在的卡 ⇒ 空清单，不是错误");
}

/// 指针清单的**顺序**是契约的一部分：`episode` 镜像卡的 `pointers_json` 就是写侧按事件序
/// 盖下的成员序列（未知事件时间的排后），`ORDER BY p.key` 把它原样读回。
#[test]
fn card_fact_pointers_按数组序给裸_id_未知时间的排后() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let late = fact_and_card(m, 0, "晚发生的", SRC0).0;
    let early = fact_and_card(m, 1, "早发生的", SRC0 + 30 * MIN).0;
    let undated = fact_and_card(m, 2, "没日期的", SRC0 + 40 * MIN).0;
    set_occurred(m, &late, Some(SRC0 + 10 * MIN));
    set_occurred(m, &early, Some(SRC0 + MIN));
    // 交入序故意打乱：读回来的序是**库里那一行**说的序，不是调用方递的顺序。
    put_episodes(m, &[episode("三段经历", "两段时间已知一段未知。",
                              vec![late.clone(), undated.clone(), early.clone()])],
                 &ctx(), NOW).unwrap();
    let mirror = compute_episode_card_id(&scope(), &[early.clone(), late.clone(), undated.clone()]);
    assert_eq!(reader::card_fact_pointers(m, &mirror).unwrap(),
               vec![early.clone(), late.clone(), undated.clone()],
        "已知在前（升序）、未知在后：这一格就是 mates 的时间轴");
}

// ── edges_touching ──────────────────────────────────────────────────────────────

/// 一条查询覆盖两臂：给 from 端点、给 to 端点，都要读到同一条边；结果按
/// `created_at_ms DESC, id`（与 `event_link_of` 同一把"最新先看"的尺子）。
#[test]
fn edges_touching_一条查询覆盖_from_与_to_两臂且最新先看() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = card(m, "甲卡", "甲卡的正文", 'a');
    let b = card(m, "乙卡", "乙卡的正文", 'b');
    let c = card(m, "丙卡", "丙卡的正文", 'c');
    // 结构关系那一支不要求机制句（P-4），三条边分别落在 from/to 两种位置与三个时刻。
    assert_eq!(put_edges(m, &[edge(&a, &b, "depends_on", "先看甲", vec![])], &ctx(), NOW + MIN)
        .unwrap(), 1);
    assert_eq!(put_edges(m, &[edge(&b, &c, "depends_on", "先看乙", vec![])], &ctx(), NOW + 2 * MIN)
        .unwrap(), 1);
    assert_eq!(put_edges(m, &[edge(&c, &a, "located_in", "先看丙", vec![])], &ctx(), NOW + 3 * MIN)
        .unwrap(), 1, "夹具的三条边必须真的落库");

    // from 臂：甲作为起点的边、以及 to=甲 的那条，都要命中。
    let from_arm = reader::edges_touching(m, &[format!("card:{a}")], 10).unwrap();
    assert_eq!(from_arm.len(), 2, "from=甲 一条、to=甲 一条，两臂都要命中");
    assert_eq!(from_arm[0].created_at_ms, NOW + 3 * MIN, "created_at_ms DESC：最新那条在前");
    assert_eq!((from_arm[0].to_kind.as_str(), from_arm[0].to_id.as_str()), ("card", a.as_str()),
        "最新那条是 丙→甲（to 臂命中）");
    assert_eq!((from_arm[1].from_kind.as_str(), from_arm[1].from_id.as_str()), ("card", a.as_str()),
        "次新那条是 甲→乙（from 臂命中）");
    // `EdgeFacts` 的两端是**拆过的前缀 + 裸 id**（`split_endpoint` 那一格）：库里的键是
    // `card:<id>`，读数里 `from_kind`/`to_kind` 是前缀、`from_id`/`to_id` 是裸 id。
    // 查询按库里的键给端点，读数按拆开的两格比对——两侧混用就是"读不到"那一形。

    // to 臂：给中继端点乙，两个方向各命中一条（甲→乙 与 乙→丙）。
    let to_arm = reader::edges_touching(m, &[format!("card:{b}")], 10).unwrap();
    assert_eq!(to_arm.len(), 2, "同一批端点里 from=乙 与 to=乙 各一条，两臂都算触达");
    assert_eq!((to_arm[0].from_kind.as_str(), to_arm[0].from_id.as_str()), ("card", b.as_str()),
        "新的在前：乙→丙（NOW+2min）");
    assert_eq!(to_arm[0].kind, "depends_on", "relation 落在 kind 那一格");
    assert_eq!((to_arm[1].to_kind.as_str(), to_arm[1].to_id.as_str()), ("card", b.as_str()),
        "次新的是 甲→乙（NOW+1min）");

    // limit 由调用方夹紧：给 1 就只回 1 条，且是**最新**那条（不是任意一条）。
    let capped = reader::edges_touching(m, &[format!("card:{a}"), format!("card:{c}")], 1).unwrap();
    assert_eq!(capped.len(), 1);
    assert_eq!(capped[0].created_at_ms, NOW + 3 * MIN);
}

/// 空端点集**不许**拼出 `IN ()`（SQLite 的语法错会被读成"库坏了"，而它的意思只是"没问"）。
#[test]
fn edges_touching_空端点返回空集而不是语法错() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = card(m, "甲卡", "甲卡的正文", 'a');
    let b = card(m, "乙卡", "乙卡的正文", 'b');
    put_edges(m, &[edge(&a, &b, "depends_on", "先看甲", vec![])], &ctx(), NOW).unwrap();
    assert!(reader::edges_touching(m, &[], 10).unwrap().is_empty(),
        "没给端点就是没问：空集，零条 SQL 的语法面");
}

/// 深 2 的形状：第二跳把第 1 跳读到的端点**并成一个集合再查一次**。
/// 这一条断的是"第二跳读得到、且没有把第一跳那条重复吐回来"（逐点查会漏掉
/// 跨两跳的那条边，N+1 的形状则在条数上露不出来，所以序与去重才是这一格的牙）。
#[test]
fn edges_touching_第二跳是并端点批量再查一次() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = card(m, "起点卡", "起点卡的正文", 'a');
    let b = card(m, "中继卡", "中继卡的正文", 'b');
    let c = card(m, "远端卡", "远端卡的正文", 'c');
    let hop1 = put_edges(m, &[edge(&a, &b, "depends_on", "甲乙相连", vec![])], &ctx(), NOW).unwrap();
    let hop2 = put_edges(m, &[edge(&b, &c, "depends_on", "乙丙相连", vec![])], &ctx(), NOW + MIN)
        .unwrap();
    assert_eq!((hop1, hop2), (1, 1));

    // 第 1 跳：只有甲乙那条。
    let first = reader::edges_touching(m, &[format!("card:{a}")], 3).unwrap();
    assert_eq!(first.len(), 1, "第 1 跳不许读到乙丙那条");
    assert_eq!((first[0].from_kind.as_str(), first[0].from_id.as_str()), ("card", a.as_str()));
    assert_eq!((first[0].to_kind.as_str(), first[0].to_id.as_str()), ("card", b.as_str()),
        "它把中继端点带回来，第二跳从它出发");
    // 第 2 跳：端点集合 = 第 1 跳的两端**重新拼回库里的键**（`kind` + `:` + 裸 id），
    // 一次批量查询。认不出前缀的那一形（`kind` 空串）原样带着裸 id：它在消费面的闸上
    // 过不了（fail-closed），这里不许替它猜一张表。
    let mut endpoints: Vec<String> = vec![format!("card:{a}")];
    for e in &first {
        for (kind, id) in [(&e.from_kind, &e.from_id), (&e.to_kind, &e.to_id)] {
            let key = if kind.is_empty() { id.clone() } else { format!("{kind}:{id}") };
            if !endpoints.contains(&key) {
                endpoints.push(key);
            }
        }
    }
    let second = reader::edges_touching(m, &endpoints, 3).unwrap();
    let mut both = ids_of(&second);
    both.sort();
    assert_eq!(both.len(), 2, "一次批量查询把两跳都读回来（不是逐点各查一次）");
    assert!(second.iter().any(|e| e.from_id == b && e.to_id == c),
        "远端那条边（乙→丙）在这一跳出现");
    assert!(second.iter().any(|e| e.id == first[0].id), "第一跳那条也在同一批读数里，没被去重去掉");
    assert_ne!(second[0].id, second[1].id, "两条边两个 id：端点集合里没有重复行");
}

/// 边的 `retired` 读数**不在这里筛**（与 `edges()` 同一口径：带那一格、由消费面决定不渲）。
#[test]
fn edges_touching_带_retired_读数而不在库侧筛掉退场边() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = card(m, "甲卡", "甲卡的正文", 'a');
    let b = card(m, "乙卡", "乙卡的正文", 'b');
    put_edges(m, &[edge(&a, &b, "depends_on", "先看甲", vec![])], &ctx(), NOW).unwrap();
    let live = reader::edges_touching(m, &[format!("card:{a}")], 3).unwrap();
    assert_eq!(live.len(), 1);
    assert!(!live[0].retired, "没有退场账 ⇒ 读 false");
    retire_event_link(m, &live[0].id, "判同撤销", NOW + MIN).unwrap();
    let after = reader::edges_touching(m, &[format!("card:{a}")], 3).unwrap();
    assert_eq!(after.len(), 1, "退场的边仍然被读回来（死活由消费面判，两个读面一套口径）");
    assert!(after[0].retired);
}

/// 「边证据」那一格：`EdgeFacts` 现在带 `evidence_json`。
/// 两族形状都断——`put_edges` 那一族恒 `'[]'`（P-1：证据落 `basis`，写了没人读的列不许当闸），
/// A44 软边那一族是**真数组**（`put_timeline_rows_counted` 写 `json_array(basis_ids)`）。
#[test]
fn 边证据两族形状_空数组与被锚行数组() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let a = card(m, "甲卡", "甲卡的正文", 'a');
    let b = card(m, "乙卡", "乙卡的正文", 'b');
    put_edges(m, &[edge(&a, &b, "depends_on", "先看甲", vec![])], &ctx(), NOW).unwrap();
    let plain = reader::edges_touching(m, &[format!("card:{a}")], 3).unwrap();
    assert_eq!(plain[0].evidence_json, "[]",
        "put_edges 那一族的 evidence_json 就是写死的空数组（证据在 basis 那一格）");
    assert_eq!(plain[0].reason, "先看甲", "basis 列的读数落在 reason 那一格");

    let (rows, links) = put_timeline_rows_counted(m, &[TimelineContent {
        title: "derive-发布排在季度会之后".into(), body: "发布排在季度会之后".into(),
        scope: scope(), time_anchor_ms: Some(NOW + 1), time_anchor_id: Some(a.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![a.clone()],
    }], &ctx(), NOW + 2 * MIN).unwrap();
    assert_eq!((rows, links), (1, 1), "夹具的软边必须真的落库");
    let soft = reader::edges(m).unwrap().into_iter()
        .find(|e| e.kind == "responds_to").expect("软边在库里");
    assert_eq!(soft.evidence_json, format!("[\"{a}\"]"),
        "A44 软边那一族读得到被锚行的 id 数组（它才是 evidence_json 的真生产者；写侧存的是裸 id）");
}

// ── card_modes ──────────────────────────────────────────────────────────────────

/// 两列的批量侧查：抽取行 = `stated` + `unknown`（`writers_x::card_sql` 那一格的原文），
/// 经历镜像卡 = `inferred` + `speech`（`episode_time_arm` 的第二支）。
/// **锚行不在返回里**：那张表结构上没有这两列（同 `select_sql` 给它补字面量的那一族理由）。
#[test]
fn card_modes_批量两列_三支读数各归各位() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let (fa, stated_card) = fact_and_card(m, 0, "说了甲", SRC0 + 7 * MIN);
    let (fb, _) = fact_and_card(m, 1, "说了乙", SRC0 + 2 * MIN);
    // 成员全无事件时间 ⇒ 镜像卡退到发话时刻并标 'speech'（Phase 4 渲染要分辨的那一支）。
    put_episodes(m, &[episode("一段没有日期的经历", "只记得说过。", vec![fa.clone(), fb.clone()])],
                 &ctx(), NOW).unwrap();
    let mirror = compute_episode_card_id(&scope(), &[fb.clone(), fa.clone()]);

    let mut got: Vec<CardMode> = reader::card_modes(m, &[stated_card.clone(), mirror.clone()])
        .unwrap();
    got.sort_by(|a, b| a.id.cmp(&b.id));
    let read = |id: &str| got.iter().find(|c| c.id == id).unwrap_or_else(|| panic!("{id} 没被读回来"));
    assert_eq!(read(&stated_card).mode.as_deref(), Some("stated"),
        "抽取行吃列的 DEFAULT 'stated'（不是 inferred）");
    // 两族陈述面卡的写法不同，这一格跟着分岔（实测，两条都有真生产者）：
    // ① `writers_fact::CARD_INSERT` 的列单里**根本没有** time_confidence ⇒ 列取 NULL；
    // ② `writers_x::card_sql`（put_card 那一族）显式写 `NULL,NULL,NULL,'unknown'`（裁定 13）。
    // 消费面对这两形走同一支渲染（NULL 与 'unknown' 都是"沿用既有那一支"），
    // 所以这里断的是**库里那一格的原文**，不是渲染口径。
    assert_eq!(read(&stated_card).time_confidence, None,
        "writers_fact 的最小消费卡不写这一列 ⇒ NULL（它没有可重算的锚）");
    assert_eq!(read(&mirror).mode.as_deref(), Some("inferred"),
        "派生族显式写 inferred，不吃 DEFAULT（Task 7 的分层可信度）");
    assert_eq!(read(&mirror).time_confidence.as_deref(), Some("speech"),
        "'speech' = 那一格的钟是发话钟，不是事件时间");

    // ② 那一族：put_card 写的卡显式带 'unknown'。
    let plain = card(m, "决策卡", "决策卡的正文", 'k');
    let plain_read = reader::card_modes(m, std::slice::from_ref(&plain)).unwrap();
    assert_eq!(plain_read[0].mode.as_deref(), Some("stated"), "put_card 吃列的 DEFAULT");
    assert_eq!(plain_read[0].time_confidence.as_deref(), Some("unknown"),
        "writers_x::card_sql 那一条 INSERT 显式写 'unknown'（裁定 13 的无锚形状）");

    // 空入参不拼 IN ()；不存在的 id 不出现在返回里（不是"读出一行 NULL"）。
    assert!(reader::card_modes(m, &[]).unwrap().is_empty());
    let ghost = reader::card_modes(m, &["g".repeat(64), stated_card.clone()]).unwrap();
    assert_eq!(ghost.len(), 1, "认不出的 id 不许被补成一行读数");
    assert_eq!(ghost[0].id, stated_card);
}

/// `RowFacts` 一个字段都不许加（14 处结构体字面量），所以这一族的**唯一**读法是侧查。
/// 这一条把它钉成读数：同一批 id 两次调用逐字相同，且锚行永远读不出这两列。
#[test]
fn card_modes_锚行不在列面上_两次调用逐字相同() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let (fa, stated_card) = fact_and_card(m, 0, "重复读数", SRC0);
    let anchor_id = anchor(0);
    let first = reader::card_modes(m, &[stated_card.clone(), anchor_id.clone()]).unwrap();
    let second = reader::card_modes(m, &[anchor_id.clone(), stated_card.clone()]).unwrap();
    assert_eq!(first.len(), 1, "锚行没有这两列 ⇒ 它整个不出现在返回里（不补 NULL 行）");
    assert_eq!(first, second, "顺序与入参顺序无关：按 id 升序的稳定读数");
    assert_eq!(first[0].id, stated_card);
    // 锚行确实活着（否则上面那句"不出现"就只是"没这一行"，而不是"没有这两列"）。
    let n: i64 = m.conn().query_row("SELECT count(*) FROM anchor WHERE id=?1", [&anchor_id],
                                   |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "前置：锚行在场");
    assert!(!fa.is_empty());
}

/// mention 那一条读面与本层无关，但 `edges_touching` 的端点**可能是 mention 边的行**吗？
/// 不是：mention 落 `memory_mention`，不落 `fact_link`。这一条断的是端点形状的另一半——
/// `anchor:` 前缀的端点在库里今天不出现，给了也只是空集（不许报错）。
#[test]
fn edges_touching_给没有边的端点返回空集() {
    let (_dir, root) = seeded();
    let m = root.memory();
    assert!(reader::edges_touching(m, &[format!("anchor:{}", anchor(0))], 3).unwrap().is_empty());
    let a = card(m, "甲卡", "甲卡的正文", 'a');
    let b = card(m, "乙卡", "乙卡的正文", 'b');
    put_mentions(m, &[MentionContent { row_id: a.clone(), name: "乙卡".into() }], &ctx(), NOW)
        .unwrap();
    assert!(reader::edges_touching(m, &[format!("card:{a}")], 3).unwrap().is_empty(),
        "mention 不是 fact_link 的端点：有 mention 边不等于有关系边");
    put_edges(m, &[edge(&a, &b, "depends_on", "先看甲", vec![])], &ctx(), NOW).unwrap();
    assert_eq!(reader::edges_touching(m, &[format!("card:{a}")], 3).unwrap().len(), 1,
        "阳性对照：同一批端点，真落了边就读得到");
}

/// 镜像卡句柄的形状：`MemKey` 的表变体恒为 `Card`（`MemTable` 不扩，episode 的稳定句柄就是卡）。
#[test]
fn 过闸句柄的表变体只有_card() {
    let (_dir, root) = seeded();
    let m = root.memory();
    let (fa, stated) = fact_and_card(m, 0, "句柄形状", SRC0);
    let key = reader::card_for_fact(m, &fa).unwrap().expect("陈述面卡在场");
    assert!(matches!(key, MemKey { table: MemTable::Card, .. }));
    assert_eq!(key.key_string(), format!("card:{stated}"),
        "句柄串就是 resource_owner 的两列拼法（过闸用的那个键）");
}
