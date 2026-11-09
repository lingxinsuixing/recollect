//! G4 出口判据（架构 §6 G4①）：落了推导层、再按冻结配方删光它 ⇒ 事实族四表行一字不变、仍可检索。
//!
//! "推导层" = `relationship` 全行 + `event` 表里 `kind IN ('causal','timeline')` 的行
//! + `provenance` 表里 `ref_kind != 'turn'` 的行 + `mention` 全行
//!   （批次 M 合证面：mention 是"某事实行提到了某实体"的派生标注，随推导层一起可丢——丢了不丢一句原文）。
//!
//! 快照取**三次**、断言两支全等，因为判据要能说两件不同的事：
//! ① 落推导层**不改**事实行——写侧越界（推导路径"顺手"改依据行）只有在"落库前后"这一对上才会红；
//!    只比"删前后"的话，基线快照本身是越界之后拍的，那条断言绿得没有理由（空过）。
//! ② 删推导层**不改**事实行——删侧误伤。
//! 再加③ 读侧不抛（`facts` / `dangling_attestations`）：那才是"可检索"这一承诺的载体。
//! 两支主判据不把「推导表清零」当判据（清零是手段）；批次③ 段 C-2 另加一条**独立**断言，
//! 钉的是冻结配方跑完必须带走链头行与它的全部 link/basis 行（验收 3）。

use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::reader::{self, RowFacts};
use rc_memory::writers_x::{put_assertion, put_card, put_chains, put_edges, put_entity,
                           put_event, put_mentions, put_timeline_rows, BatchCtx, ChainContent,
                           EdgeContent, EntityContent, FactContent, MentionContent,
                           TimelineContent};
use rc_memory::Provenance;
use rc_store::MemoryHandle;

/// 真库小帮手（`crates/rc-index/tests/rebuild.rs` 的 `memory_handle` 同形；tests/*.rs 互不引用，
/// 所以这四行在本文件里就地写一遍）。`TempDir` 必须与句柄成对返回：它一掉库文件就没了。
fn memory_handle() -> (tempfile::TempDir, MemoryHandle) {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    (dir, root.into_memory())
}

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "p-synthetic".into() } }

/// 轮次 uid 的夹具形状与既有文件同档（`"t".repeat(64)` 那一族）：真 uid 是内容哈希，
/// 短记号 `"turn-1"` 会让用例只在"形状不对的 uid"上绿（P-29）。
fn turn_uid(seed: char) -> String { seed.to_string().repeat(64) }

/// 一行出处。空出处在这儿不是省事而是当场红：`writers_x::validate_fields` 对任何
/// `put_*` 都先查 `ProvenanceMissing`，推导事件的夹具行也一样。
fn prov(seed: char, role: AnchorRole) -> Vec<Provenance> {
    vec![Provenance { turn_uid: turn_uid(seed), anchor_role: role }]
}

/// 链头挂着的出处行数（link + basis 全算）。G4 换到 `put_chains` 之后，
/// "配方带走全链"的证人就是这个计数在删前后从 3 变 0——不是"第三支自己删了自己 WHERE 里
/// 的东西"那种恒真读数：变异形（把第三支的 `ref_kind != 'turn'` 改窄成 `= 'card'`）留下
/// 两行 `ref_kind='entity'` 的链行，本计数当场红（变异电池 M6 已真造过这一形）。
fn chain_prov_rows(m: &MemoryHandle, head: &str) -> i64 {
    m.conn().query_row(
        "SELECT count(*) FROM provenance WHERE mem_table='card' AND mem_id=?1",
        [head], |r| r.get(0)).unwrap()
}

/// `mention` 的行数。它两头都要用：落库后 `== 2` 是**非空锚**（没有它，"删后为 0"在空集上恒真），
/// 删后 `== 0` 是第四支真的带走了它们的读数。
fn mention_rows(m: &MemoryHandle) -> i64 {
    m.conn().query_row("SELECT count(*) FROM memory_mention", [], |r| r.get(0)).unwrap()
}

/// 推导两族的判据：`event` 表里 kind 是保留字的那几行。
/// 只有 `entity`/`card`/`event`(非保留字)/`assertion` 进快照。
fn is_derived(r: &RowFacts) -> bool {
    r.key.table == MemTable::Card && (r.kind == "causal" || r.kind == "timeline")
}

/// 事实族逐字段快照。载体是 `reader::facts` 的 `RowFacts` Debug 文本——四张表的内容列、
/// 时间列与四个判定标量列（`retired`/`rejected`/`attestation`/`anchor_role`）都在里面，
/// 而 `anchor_role` 正是从 `provenance` 算的：出处行被误删它立刻漂 ⇒ 这一格有牙。
/// 手抄一份 UNION SQL 反而弱：抄漏一列少一格判据，而 `entity` 没有正文列，
/// 照 card 的写法给它 `SELECT body` 是当场语法错误。
fn snapshot(m: &MemoryHandle) -> Vec<String> {
    reader::facts(m).unwrap().iter()
        .filter(|r| !is_derived(r))
        .map(|r| format!("{r:?}"))
        .collect()
}

#[test]
fn deleting_derived_layer_keeps_fact_rows_unchanged_and_readable() {
    let (_dir, m) = memory_handle();
    let now = 1_700_000_000_000i64;
    // ===== 事实族五行（四张表都在场）=====
    let e1 = put_entity(&m, &EntityContent { kind: "concept".into(), title: "alpha".into(),
                                             scope: scope(), aliases: vec![] },
                        &prov('e', AnchorRole::Human), now).unwrap().id;
    let e2 = put_entity(&m, &EntityContent { kind: "concept".into(), title: "beta".into(),
                                             scope: scope(), aliases: vec![] },
                        &prov('f', AnchorRole::Agent), now).unwrap().id;
    let c1 = put_card(&m, &FactContent { kind: "decision".into(), title: "那条决定".into(),
                                         body: "任何合并之前先跑全量测试。".into(),
                                         scope: scope(), valid_to_ms: None },
                      &prov('c', AnchorRole::Human), now).unwrap().id;
    let e3 = put_event(&m, &FactContent { kind: "meeting".into(), title: "会议记录".into(),
                                          body: "会上定了那条决定。".into(),
                                          scope: scope(), valid_to_ms: None },
                       &prov('v', AnchorRole::System), now).unwrap();
    put_assertion(&m, &FactContent { kind: "claim".into(), title: "那条断言".into(),
                                     body: "那条决定到明天还成立。".into(),
                                     scope: scope(), valid_to_ms: None },
                  &prov('a', AnchorRole::Human), now).unwrap();
    let before = snapshot(&m);
    // **E43 补：非空锚。** 下面三条断言都是 `Vec == Vec`，而 `snapshot()` 是**过滤后**的——
    // 过滤器一旦写坏（比如 `is_derived` 恒真、把 Card 也算推导行），三条断言都在空集上比较，
    // **恒等、恒绿**，整道 G4 闸门 fail 不掉（E43 实测：这两种改法都照旧 1 passed）。
    // `facts.len()==5` 那一格读的是**未过滤**数据，救不了这一格 ⇒ 非空锚只能落在快照自己身上。
    assert_eq!(before.len(), 5, "快照空了 ⇒ 这道 G4 闸门在空集上恒等，是空过的守卫");

    // ===== 推导层四支（四支全部走生产写入口）=====
    // ① 边：`put_edges` 落一行 `relationship` + 两端各一行 `ref_kind='entity'` 的出处行（本夹具无第三依据）。
    put_edges(&m, &[EdgeContent { from_id: e1.clone(), to_id: e2.clone(), kind: "part_of".into(),
                                  reason: "beta 是 alpha 的前置检查。".into(), scope: scope(),
                                  basis_ids: vec![] }],
              &BatchCtx::synthetic(), now).unwrap();
    // ② 时间事件：`put_timeline_rows` 落 event 行（kind='timeline'）+ 三锚列 +
    //    一行 `ref_kind='event'` 的出处行 + 一条 `event_link` 软边。
    put_timeline_rows(&m, &[TimelineContent {
        title: "那条时间线".into(), body: "那条决定排在发布前面。".into(), scope: scope(),
        time_anchor_ms: Some(now + 86_400_000), time_anchor_id: Some(e3.id.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![],
    }], &BatchCtx::synthetic(), now).unwrap();
    // ③ 因果链：批次③ 段 B 起这一族**有**生产写入口 `put_chains`（原先那句"没有写入口、
    //    只能手工落一行"已过期）。头行落 `event(kind='causal')`，环节与依据落 `provenance`
    //    的 `role='link'`（带 `ord`）与 `role='basis'` 行；这些出处行的 `ref_kind` 全部是
    //    **被引行的表前缀**（`insert_derived_provenance` 递的是 `link_table.prefix()` /
    //    `basis_table.prefix()`，`MemTable` 四值里没有 `turn`）。依据选 e2（不在环节里）
    //    ⇒ 那一跳真落一行 basis，链头因此挂着 2 link + 1 basis = 3 行出处。
    put_chains(&m, &[ChainContent { mechanism: "那次发布引出那条决定。".into(),
                                    link_ids: vec![e1.clone(), c1.clone()],
                                    basis_ids: vec![e2.clone()],
                                    causal_type: "causes".into(), scope: scope() }],
               &BatchCtx::synthetic(), now).unwrap();
    let links_before: i64 = m.conn().query_row(
        "SELECT count(*) FROM fact_link WHERE relation='responds_to'", [], |r| r.get(0)).unwrap();
    assert_eq!(links_before, 1,
        "夹具前提：时间行锚在事件上必须真落一条软边——否则删后的零是空集读数");
    let causal_head: String = m.conn().query_row(
        "SELECT id FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap();
    assert_eq!(chain_prov_rows(&m, &causal_head), 3,
        "夹具前提：链头的两行环节 + 一行依据必须都在场，否则下面「全链消失」那条断言建在空集上");

    // ④ mention（批次 M 合证面）：段 B 起这一族**有**生产写入口 `put_mentions`，四支全部走生产入口。
    //    两行 = 同一实体 e2 被两个不同的行提到（`entity` 表的 e1 与 `card` 表的 c1），
    //    不是同一行标注两次。注意：G4 不走 parse，所以 `"beta"` 不必逐字出现在 e1/c1 的正文里——
    //    verbatim 是 parse 层模型面规则，写侧只做名字→实体解析（防下一个人以为写侧漏了校验）。
    let mo = put_mentions(&m, &[
        MentionContent { row_id: e1.clone(), name: "beta".into() },
        MentionContent { row_id: c1.clone(), name: "beta".into() },
    ], &BatchCtx::synthetic(), now).unwrap();
    assert_eq!((mo.written, mo.unresolved), (2, 0), "夹具前提：两行 mention 必须都落下");
    assert_eq!(mention_rows(&m), 2,
        "夹具前提：两行 mention 必须都落下——否则下面「删后为 0」在空集上恒真");

    // 断言①的前半：落推导层不改事实行（Step 8 的变异就是冲这一格来的）
    let after_derive = snapshot(&m);
    assert_eq!(before, after_derive, "落推导层改了事实族的行 ⇒ 推导写路径越界");

    // ===== 删光推导层（冻结配方五支，顺序不能换）=====
    m.conn().execute("DELETE FROM fact_link", []).unwrap();
    m.conn().execute("DELETE FROM card WHERE kind IN ('causal','timeline')", []).unwrap();
    m.conn().execute("DELETE FROM provenance WHERE ref_kind != 'turn'", []).unwrap();
    m.conn().execute("DELETE FROM memory_mention", []).unwrap();
    // 批次③ 新增断言（钉验收 3）：配方跑完 ⇒ 链头行 + 它的全部 link/basis 行一起消失。
    // 原先那段「手写 put_event 自带一行 `ref_kind='turn'` 出处、第三支带不走 ⇒ 库里留一张账、
    // 零行，要清干净得第四支 DELETE」整段作废——换成 `put_chains` 后链头的出处行**全部**带
    // 表前缀的 `ref_kind`（上面③的注释），一行 `turn` 都不带 ⇒ 第三支正好全带走，
    // 残值与"要第四支"那句推论一起没了（批次 M 起配方确有第四支，但那支删的是 `mention`，
    // 与出处残值无关——见上面④）。这两条读数是这一格的直接证人，也是变异电池 M6
    // （把第三支 WHERE 改窄成 `ref_kind = 'card'`）的红点：那一形下这里剩两行
    // `ref_kind='entity'` 的链行，当场红。
    let head_rows: i64 = m.conn().query_row(
        "SELECT count(*) FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap();
    assert_eq!(head_rows, 0, "链头行没被第二支（保留字 DELETE）带走");
    assert_eq!(chain_prov_rows(&m, &causal_head), 0,
        "配方五支跑完，链头的 link/basis 行一行都不许留 ⇒ 剩一行就是删除配方漏了因果链的出处族");
    assert_eq!(mention_rows(&m), 0,
        "配方五支跑完，mention 必须清零 ⇒ 剩一行就是删除配方漏了合证标注（推导层没删干净）");
    let links_after: i64 = m.conn().query_row("SELECT count(*) FROM fact_link", [], |r| r.get(0)).unwrap();
    assert_eq!(links_after, 0, "配方五支跑完，软边必须清零 ⇒ 剩一行就是删除配方漏了 A44");

    // 断言①的后半：删推导层不改事实行
    let after_delete = snapshot(&m);
    assert_eq!(before, after_delete, "事实族四表行在删推导层前后必须逐字段全等");

    // 断言②：可检索——四臂 UNION 读得通，且只剩那五行
    let facts = reader::facts(&m).unwrap();
    assert_eq!(facts.len(), 5, "清完推导层应剩五行（两实体一卡一事件一断言），拿到 {}", facts.len());
    // 断言③：悬挂表态是读数不是错误（这一格没写 attestation，只为证明读侧不抛）
    let _dangling = reader::dangling_attestations(&m).unwrap();
}
