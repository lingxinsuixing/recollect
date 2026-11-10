//! 写侧的证据闸（派发档 I-2 · Phase 2.4）。
//!
//! P-1 的那一格裁定：证据落 **`fact_link.basis` + `role='basis'` 出处行**，不落 `evidence_json`。
//! 理由不是"那格不好看"，是**那格没有读者**：`put_edges_inner` 把它写死 `'[]'`，而读侧
//! `reader::edges` / `EdgeFacts` 根本不读它（只有 `event_link_of` 读）。把闸门建在一格写了没人读的
//! 列上，就是本次刚在 `card.time_json` 上抓到的同一个缺陷形状，不许再来一次——所以本文件既断言
//! "机制句逐字进了 `basis`"，也断言"`evidence_json` 仍是 `'[]'`"（证明闸没有偷偷改地方落）。
//!
//! P-4 的那一格裁定：闸**只拦机制类 relation**（`causes` / `enables` / `prevents` / `responds_to`），
//! 结构归属那一支（`part_of` / `depends_on` / `owns` / `located_in` / `role`）不许被要求机制句，
//! 否则得到的是一批假拒。本文件用一条**五支结构关系全落库**的用例把这一格钉住。
//!
//! P-3 的那一格已知条件写死在这里：`put_edges_anchored` 的 I4 门槛判据①是"两端共享 mention"，
//! 与证据闸是**两道不同的闸**。所以每条用例都先把共享 mention 铸出来（`put_mentions` 让两端指向
//! 同一个实体），让 I4 恒过；这样"不落"只能归因于证据闸，"落了"只能归因于闸放行。

use rc_memory::keys::{Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_edges, put_edges_anchored, put_entity, put_mentions,
                           put_timeline_rows_counted, BatchCtx, EdgeContent, EntityContent,
                           FactContent, MentionContent, TimelineContent};
use rc_memory::{AnchorRole, Provenance};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-evidence".into() } }
fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}
fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() }
}

fn card(m: &rc_store::MemoryHandle, title: &str, body: &str, c: char) -> String {
    put_card(m, &FactContent { kind: "decision".into(), title: title.into(), body: body.into(),
                               scope: scope(), valid_to_ms: None }, &prov(c), NOW).unwrap().id
}

/// I4 判据①的锚条件**先满足**：一个实体 + 两张正文逐字提到它的卡 + 两条 mention。
/// 返回 `(依据行 id, from 端 id, to 端 id)`——`basis_ids` 由调用方决定给不给。
fn anchored_pair(m: &rc_store::MemoryHandle) -> (String, String, String) {
    put_entity(m, &EntityContent { kind: "concept".into(), title: "额度封顶".into(),
                                   scope: scope(), aliases: vec![] }, &prov('e'), NOW).unwrap();
    let basis = card(m, "封顶先落地", "额度封顶在 11-04 落地。", 'c');
    let a = card(m, "阈值改动", "额度封顶之后不再按 384K 触发。", 'a');
    let b = card(m, "回退链", "额度封顶也在回退链里。", 'b');
    put_mentions(m, &[MentionContent { row_id: a.clone(), name: "额度封顶".into() },
                      MentionContent { row_id: b.clone(), name: "额度封顶".into() }],
                 &ctx(), NOW).unwrap();
    (basis, a, b)
}

fn edge(from: &str, to: &str, kind: &str, basis: &str, basis_ids: Vec<String>) -> EdgeContent {
    EdgeContent { from_id: from.into(), to_id: to.into(), kind: kind.into(),
                  reason: basis.into(), scope: scope(), basis_ids }
}

fn link_count(m: &rc_store::MemoryHandle) -> i64 {
    m.conn().query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap()
}

/// 阳性对照（P-1 的落点）：机制类 relation + 说得出事情的机制句 + 点名的依据行 ⇒ 落库，
/// 机制句**逐字**进 `fact_link.basis`，`evidence_json` 仍是被写死的那一格 `'[]'`，
/// 依据行与两端一起以 `role='basis'` 出现在 `provenance` 里。
/// 这一条绿，下面几条"不落"才只可能是闸造成的。
#[test]
fn a_mechanism_edge_with_a_sentence_and_a_named_basis_row_lands_in_basis() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let (basis, a, b) = anchored_pair(m);
    let sentence = "先落地的封顶决定取消了旧的容量阈值，所以回退不再按容量触发";

    let (written, dropped) = put_edges_anchored(
        m, &[edge(&a, &b, "causes", sentence, vec![basis.clone()])], &ctx(), NOW).unwrap();
    assert_eq!((written, dropped), (1, 0), "机制句 + 依据行齐全的因果边必须落（阳性对照）");

    let got: (String, String, String) = m.conn().query_row(
        "SELECT relation, basis, evidence_json FROM fact_link", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(got.0, "causes");
    assert_eq!(got.1, sentence, "P-1：机制句落在被人读的 `basis` 那一格，逐字不截断");
    assert_eq!(got.2, "[]",
        "P-1：闸不建在 `evidence_json` 上——那一格既没被写也没被读，动它就是第二次 time_json");

    let roles: Vec<(String, String, Option<i64>)> = {
        let mut st = m.conn().prepare(
            "SELECT turn_uid, role, ord FROM provenance WHERE mem_table='fact_link'
             ORDER BY turn_uid").unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
          .unwrap().map(|x| x.unwrap()).collect()
    };
    assert_eq!(roles.len(), 3, "两端 + 一条依据 ⇒ 三行出处");
    assert!(roles.iter().all(|(_, role, _)| role == "basis"),
        "边的出处行一律 `role='basis'`（这一族没有环节行）");
    assert!(roles.iter().all(|(_, _, ord)| ord.is_none()),
        "`role='basis'` 配 NULL `ord`：那条跨列 CHECK 的另一半");
    assert!(roles.iter().any(|(uid, _, _)| uid == &basis),
        "点名的依据行必须自己占一行出处，否则「这条边凭什么」在库里读不回来");
}

/// 阴性一：机制句整串是套话（"时间相邻"）。端点的锚条件已满足 ⇒ 不落只能归因于证据闸。
#[test]
fn a_mechanism_edge_whose_basis_is_a_placeholder_is_dropped_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let (basis, a, b) = anchored_pair(m);

    let (written, dropped) = put_edges_anchored(
        m, &[edge(&a, &b, "causes", "时间相邻", vec![basis.clone()])], &ctx(), NOW).unwrap();
    assert_eq!((written, dropped), (0, 1), "套话机制说不出机制 ⇒ 不落，且可数");
    assert_eq!(link_count(m), 0, "被闸挡下的边一个字节都不许落库");
}

/// 阴性二：机制句是话，但**一条依据行都没点名**——"没有证据就不落"的那一半。
/// 两端自己的 `role='basis'` 行不算依据：那是"这条边连着谁"，不是"这条边凭什么"。
#[test]
fn a_mechanism_edge_without_a_named_basis_row_is_dropped_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let (_, a, b) = anchored_pair(m);
    let sentence = "先落地的封顶决定取消了旧的容量阈值，所以回退不再按容量触发";

    let (written, dropped) = put_edges_anchored(
        m, &[edge(&a, &b, "enables", sentence, vec![])], &ctx(), NOW).unwrap();
    assert_eq!((written, dropped), (0, 1), "说不出凭什么 ⇒ 不落，且可数");
    assert_eq!(link_count(m), 0);
}

/// 阴性三：机制句空白（`""`）。`basis` 那一列是 NOT NULL，空串落得进库、也读得回来——
/// 库自己不会红，所以值域这一半只能由写侧把守（与 `PRONOUNS`/`check_alias` 同一条纪律）。
/// 两支 relation 各测一遍：`causes`（模型会给的那一支）与 `responds_to`
/// （时间线软边那一支，今天唯一在生产里出现的机制类 relation）。
#[test]
fn a_mechanism_edge_with_a_blank_basis_is_dropped_before_any_byte_is_written() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let (basis, a, b) = anchored_pair(m);

    for kind in ["causes", "responds_to"] {
        let (written, dropped) = put_edges_anchored(
            m, &[edge(&a, &b, kind, "", vec![basis.clone()])], &ctx(), NOW).unwrap();
        assert_eq!((written, dropped), (0, 1), "{kind} 带空机制句必须不落且可数");
    }
    assert_eq!(link_count(m), 0, "空 basis 的因果边不许在库里留下行");
}

/// P-4（不许假拒）：结构归属那一支**不要求机制句**。五支关系各带空 basis、零依据行，
/// 锚条件（共享 mention）满足 ⇒ 五支全落。撤掉这一格保护（把闸扩到全部 relation）
/// 就会在这一条红——那是本批最容易做歪的地方，所以它单独占一条用例。
#[test]
fn structural_relations_are_not_asked_for_a_mechanism_sentence() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let (_, a, b) = anchored_pair(m);

    let kinds = ["part_of", "depends_on", "owns", "located_in", "role"];
    let edges: Vec<EdgeContent> = kinds.iter()
        .map(|k| edge(&a, &b, k, "", vec![])).collect();
    let (written, dropped) = put_edges_anchored(m, &edges, &ctx(), NOW).unwrap();
    assert_eq!((written, dropped), (5, 0),
        "结构归属被证据闸误伤就是假拒（P-4）：五支必须全落");
    let relations: Vec<String> = {
        let mut st = m.conn().prepare("SELECT relation FROM fact_link ORDER BY relation").unwrap();
        st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
    };
    assert_eq!(relations.len(), 5, "五支各一行（id 由 (from, kind, to) 现算，kind 不同就是不同边）");
}

/// 库门口那一道对**绕过 parse 的调用者**同样生效（与 `put_chains_merged` 的
/// `ContentEmpty` 同一处分工：模型面住 parse，门口住写侧）。
/// `put_edges`（不带 I4 门槛的旧入口）也拦——但它只有一个返回格（写入数），
/// 那一格的挡下数今天没有出口，所以这一支断言只钉"不落行"，不钉计数。
#[test]
fn the_door_holds_for_callers_that_bypass_parse() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let (basis, a, b) = anchored_pair(m);

    let written = put_edges(m, &[edge(&a, &b, "prevents", "同一个实体", vec![basis])],
                            &ctx(), NOW).unwrap();
    assert_eq!(written, 0, "旧入口同样不许落一条说不出机制的因果边");
    assert_eq!(link_count(m), 0);
}

/// 生产里今天真会出现的那一支机制类边：时间线的 A44 软边（`relation='responds_to'`，
/// `basis` = 时间行正文）。它走的是 `put_timeline_rows_counted` 里那条旁路 SQL，
/// 不是 `put_edges_inner`——所以闸要在那一处也长一遍。
/// **上限只挡边不挡行**是既有形状（链长上限二的那条用例），本闸沿用同一处分寸：
/// 正文空白/套话 ⇒ 软边不落、`links` 读数自然少一，时间行与它的锚列照落。
#[test]
fn the_responds_to_bypass_edge_is_gated_without_dropping_the_timeline_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let anchor = card(m, "季度会", "会上定了十月发布。", 'a');

    // 阳性对照：正文是一句话 ⇒ 时间行 + 软边各一行。
    let (rows, links) = put_timeline_rows_counted(m, &[TimelineContent {
        title: "derive-发布排在季度会之后".into(), body: "发布排在季度会之后".into(),
        scope: scope(), time_anchor_ms: Some(NOW + 1), time_anchor_id: Some(anchor.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![anchor.clone()],
    }], &ctx(), NOW).unwrap();
    assert_eq!((rows, links), (1, 1), "有正文的软边照落（阳性对照）");

    // 阴性：正文空白 ⇒ 时间行落、软边不落。撤掉闸就是 links 多一。
    let (rows2, links2) = put_timeline_rows_counted(m, &[TimelineContent {
        title: "derive-".into(), body: "".into(), scope: scope(),
        time_anchor_ms: Some(NOW + 2), time_anchor_id: Some(anchor.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![anchor.clone()],
    }], &ctx(), NOW).unwrap();
    assert_eq!((rows2, links2), (1, 0), "空 basis 的 respond 边不落，但时间行不跟着丢");
    let n: i64 = m.conn().query_row(
        "SELECT count(*) FROM card WHERE kind='timeline'", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "两条时间行都在库里：闸只挡边");
    let edges: i64 = m.conn().query_row(
        "SELECT count(*) FROM fact_link WHERE relation='responds_to'", [], |r| r.get(0)).unwrap();
    assert_eq!(edges, 1, "库里只留有机制句的那一条软边");
}
