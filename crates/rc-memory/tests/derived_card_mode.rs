//! Task 7（2026-11-03 本机=中心 · 执行计划 §7）：**推导出来的记忆卡必须能被读者分辨成「推出来的」**。
//!
//! 缺陷原委：`card.mode TEXT NOT NULL DEFAULT 'stated'`（`crates/rc-store/src/ddl.rs:487`，
//! 这一列**没有 CHECK**——同表 `causal_type` 有 CHECK、别的表的 `mode` 也有 CHECK，见 ddl.rs:490
//! 与 ddl.rs:368/450，这把 grep 能查出 CHECK，所以「card.mode 没 CHECK」是真阴性不是漏搜。
//! 结论：写 `'inferred'` 不需要动 DDL），而 derive 写卡的两条 INSERT 字面量
//! （`crates/rc-memory/src/writers_x.rs` 的 timeline 与 causal）**列单里没有 `mode`**
//! ⇒ 推导卡与 L1 原文直述**同形落库**，读者无法分层信任。
//!
//! 判据一句话：**derive 写的每一张卡都必须落 `mode='inferred'`，L1 卡不受影响（仍落 `stated`）。**
//!
//! 在场路径（三条 L1 对照 + 两条推导，全部走真写入口，不手搓 SQL）：
//! - L1：`writers_x::put_card` → `card_sql()`（简报点名「不要动它，落 stated 是对的」）；
//! - L1：`writers_fact::put_fact_rows` → `CARD_INSERT`（最小消费卡，`kind=category`，
//!   既有断言 `writers_fact.rs:320` 已把它的 `mode` 钉成 `stated`，本任务不碰）；
//! - 推导：`writers_x::put_timeline_rows_counted`（kind='timeline'）；
//! - 推导：`writers_x::put_chains_merged`（kind='causal'）。
//!
//! 断言形状（简报 Step 5「可红守卫」）：主判据是**对 `card` 全表扫**，不是只查两行——
//! ② 逐行规则 + ③ 集合规则 + ④ 与 kind 无关的署名规则三层叠起来：新加一条推导写入
//! （无论它落在哪个 kind）都会踩到 ④；新加 L1 写入漂成 inferred 会踩到 ②/④；
//! 有人往本夹具里加第三条推导路径而忘了 mode，会红在 ④。
//!
//! 夹具只用通用占位名（张三/某项目），不含任何真实语料专名或 holdout。

use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_x::{
    put_card, put_chains_merged, put_timeline_rows_counted, BatchCtx, ChainContent, FactContent,
    TimelineContent,
};
use rc_memory::{compute_mem_id, AnchorRole, MemTable, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;
const DEVICE: &str = "dev-mode";
/// derive 侧署名，与 `rc_derive::derive_pass` 的那两个常量逐字同串。
const DERIVE_AGENT: &str = "rc-derive";
/// L1 侧署名（抽取/写库层），与 derive 那一族分开，④ 才分得开两侧。
const EXTRACT_AGENT: &str = "agent-mode";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-mode".into() }
}

fn derive_ctx() -> BatchCtx {
    BatchCtx { source_agent: DERIVE_AGENT.into(), device_id: DEVICE.into() }
}

fn extract_ctx() -> BatchCtx {
    BatchCtx { source_agent: EXTRACT_AGENT.into(), device_id: DEVICE.into() }
}

/// 抽取侧形状的出处 ⇒ L1 卡。
fn prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Human }]
}

fn l1_card(title: &str, body: &str) -> FactContent {
    FactContent {
        kind: "decision".into(),
        title: title.into(),
        body: body.into(),
        scope: scope(),
        valid_to_ms: None,
    }
}

/// 主体锚的句柄按生产算法算：kind/canonical/scope 就是下面 `seed_anchor` 铺下的那一行的三格
/// （`person` / `张三` / `project/proj-mode` = [`scope()`]），写卡与铺锚因此同源、不会各写一份。
fn subject_anchor() -> String {
    compute_mem_id(MemTable::Anchor, "person", "张三", "", &scope())
}

/// `fact.subject_anchor_id` 有 `REFERENCES anchor(id)` ⇒ 走 writers_fact 之前先铺锚。
fn seed_anchor(m: &MemoryHandle) {
    m.conn().execute(
        "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                            scope_key, status, profile_text, created_at_ms, updated_at_ms,
                            norm_version)
         VALUES(?1,'person','张三','unknown',NULL,'project','proj-mode','active',NULL,?2,?2,?3)",
        rusqlite::params![subject_anchor(), NOW, rc_store::capability::NORM_VERSION as i64],
    ).unwrap();
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-mode".into()),
        owner_agent_id: EXTRACT_AGENT.into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

/// `card` 全表读数：`(kind, mode, source_agent)`，按 kind、id 升序
/// ⇒ 断言不依赖插入顺序，且**新写入的卡会自动进这份读数**（Step 5 的要点）。
fn cards(m: &MemoryHandle) -> Vec<(String, String, String)> {
    let mut st = m
        .conn()
        .prepare("SELECT kind, mode, source_agent FROM card ORDER BY kind, id")
        .unwrap();
    st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
        .unwrap()
        .map(|x| x.unwrap())
        .collect()
}

/// 某个 kind 的全部 mode 读数（排序去重）。
fn modes_of_kind(rows: &[(String, String, String)], kind: &str) -> Vec<String> {
    let mut out: Vec<String> =
        rows.iter().filter(|(k, _, _)| k == kind).map(|(_, mode, _)| mode.clone()).collect();
    out.sort();
    out.dedup();
    out
}

fn count_where(rows: &[(String, String, String)], f: impl Fn(&(String, String, String)) -> bool)
    -> usize {
    rows.iter().filter(|row| f(row)).count()
}

/// 铺四张卡：两条 L1 入口各写一张（kind='decision'）+ 一张 timeline + 一张 causal。
/// 推导那两条的环节与锚必须指向真在库里的行，所以先铺 L1 卡。
fn seed_all_four_paths(m: &MemoryHandle) {
    let a = put_card(m, &l1_card("环节甲", "甲这一行的正文写着发布推迟"), &prov(), 1000).unwrap();
    let b = put_card(m, &l1_card("环节乙", "乙这一行的正文写着迁移完成"), &prov(), 2000).unwrap();

    // L1 入口②：writers_fact 的最小消费卡（kind=category='decision'）。
    seed_anchor(m);
    let item = FactWriteItem {
        subject_anchor_id: subject_anchor(),
        object_anchor_id: None,
        predicate: "decide".into(),
        value: Some("发布推迟到十二月".into()),
        category: "decision".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: "我们决定发布推迟到十二月".into(),
        evidence: vec![("t".repeat(64), AnchorRole::Human)],
        text: "张三决定把发布推迟到十二月，等迁移完成再切流量。".into(),
        scope: scope(),
        source_ms: NOW,
    };
    let out = put_fact_rows(m, &[item], &extract_ctx(), &owner(), NOW).unwrap();
    assert!(out.facts >= 1 && out.cards >= 1, "夹具前提破了：L1 fact 与最小卡都得真落行");

    // 推导入口①：timeline（锚在环节甲 ⇒ 走真写入路径，含 event_link 旁路）。
    let timeline = TimelineContent {
        title: "发布推迟一行".into(),
        body: "发布排在迁移完成之后。".into(),
        scope: scope(),
        time_anchor_ms: Some(1500),
        time_anchor_id: Some(a.id.clone()),
        time_confidence: "anchored".into(),
        basis_ids: vec![b.id.clone()],
    };
    let (written, _links) = put_timeline_rows_counted(m, &[timeline], &derive_ctx(), NOW).unwrap();
    assert_eq!(written, 1, "夹具前提破了：timeline 必须真落一行");

    // 推导入口②：causal 链头。机制句刻意不与任何环节正文相近，
    // 否则会被「机制句复述环节 ⇒ 合并」那条降权判据吞掉（那是夹具问题，不是被测行为）。
    let chain = ChainContent {
        mechanism: "先立的决定把后续排期整个顶掉了".into(),
        link_ids: vec![a.id.clone(), b.id.clone()],
        basis_ids: vec![a.id.clone()],
        causal_type: "causes".into(),
        scope: scope(),
    };
    let (n, merged) = put_chains_merged(m, &[chain], &derive_ctx(), NOW).unwrap();
    assert_eq!((n, merged), (1, 0), "夹具前提破了：causal 链头必须真落一行且没被合并");
}

/// 主判据：推导卡全落 `inferred`，L1 卡一枚都不受牵连。
#[test]
fn 推导卡落_inferred_而_l1_卡仍落_stated() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    seed_all_four_paths(m);

    let rows = cards(m);

    // ⓪ 在场性（全表 `all()` 对空集合恒真 ⇒ 先钉四类卡都真在库里，断言才不是空转）。
    assert_eq!(count_where(&rows, |r| r.0 == "timeline"), 1, "夹具里必须恰有一条 timeline 卡");
    assert_eq!(count_where(&rows, |r| r.0 == "causal"), 1, "夹具里必须恰有一条 causal 卡");
    assert!(count_where(&rows, |r| r.0 == "decision" && r.1 == "stated") >= 2,
        "夹具里必须有 L1 卡作对照（两条 L1 入口各一张）：{rows:?}");

    // ① 逐 kind 定向（变异证据要能指名是哪条写路径回归）。
    assert_eq!(modes_of_kind(&rows, "timeline"), vec!["inferred".to_string()],
        "timeline 是推导产物，落库必须可被读者分辨成 inferred");
    assert_eq!(modes_of_kind(&rows, "causal"), vec!["inferred".to_string()],
        "causal 链头是推导产物，落库必须可被读者分辨成 inferred");

    // ② 全表逐行规则（简报 Step 5 建议的形状：对 card 全表扫，而不是只查两行）。
    assert!(rows.iter().all(|(kind, mode, _)| match kind.as_str() {
        "timeline" | "causal" => mode == "inferred",
        _ => mode == "stated",
    }), "推导卡必须可被分辨成推出来的，L1 卡不受影响：{rows:?}");

    // ③ 集合规则：落成 inferred 的 kind 恰好是推导那两族。新推导路径若写 inferred
    //    会把这个断言弄红，逼实施者**有意识地**把新 kind 登记进 ② 的白名单。
    let mut inferred_kinds: Vec<&str> = rows.iter()
        .filter(|(_, mode, _)| mode == "inferred")
        .map(|(kind, _, _)| kind.as_str())
        .collect();
    inferred_kinds.sort();
    inferred_kinds.dedup();
    assert_eq!(inferred_kinds, vec!["causal", "timeline"],
        "落 inferred 的 kind 集合变了：新增推导路径先确认它确实该被分辨成推出来的，\
         再把它登记进 ② 的白名单：{rows:?}");

    // ④ 与 kind 无关的署名规则（真正的「第三条路径」牙齿）：derive 署名写的卡不许落
    //    stated（漏写 mode 就是这一条红），L1 署名写的卡不许落 inferred。
    let derive_not_inferred =
        count_where(&rows, |r| r.2 == DERIVE_AGENT && r.1 != "inferred");
    assert_eq!(derive_not_inferred, 0,
        "署名 {DERIVE_AGENT} 的卡里有没落 inferred 的 = 那条推导路径没写 mode：{rows:?}");
    let l1_not_stated = count_where(&rows, |r| r.2 != DERIVE_AGENT && r.1 != "stated");
    assert_eq!(l1_not_stated, 0, "L1 侧的卡不许被牵连成 inferred：{rows:?}");
}
