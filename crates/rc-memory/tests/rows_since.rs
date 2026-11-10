use rc_memory::{reader, keys::{Scope, ScopeKind, AnchorRole},
               put_card, FactContent, Provenance, retire,
               writers_x::{put_timeline_rows, BatchCtx, TimelineContent}};
use rc_store::Root;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() } }
fn prov() -> Vec<Provenance> { vec![Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Human }] }
fn card(kind: &str, body: &str) -> FactContent {
    FactContent { kind: kind.into(), title: format!("t-{kind}"), body: body.into(),
                  scope: scope(), valid_to_ms: None }
}

#[test]
fn none_from_returns_all_alive_rows_in_stable_order() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    put_card(root.memory(), &card("decision", "x"), &prov(), 1000).unwrap();
    put_card(root.memory(), &card("lesson",   "y"), &prov(), 2000).unwrap();
    put_card(root.memory(), &card("context",  "z"), &prov(), 3000).unwrap();
    let rows = reader::rows_since(root.memory(), None, 100).unwrap();
    assert_eq!(rows.len(), 3);
    // 排序：ORDER BY created_at_ms ASC, tbl ASC, id ASC
    let times: Vec<i64> = rows.iter().map(|r| r.created_at_ms).collect();
    assert_eq!(times, vec![1000, 2000, 3000], "rows_since 不按 created_at_ms 升序");
}

#[test]
fn limit_two_paginates_without_drop_or_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for i in 0..5 { put_card(root.memory(), &card(&format!("d{i}"), "x"), &prov(), 1000 + i).unwrap(); }
    // 第一页
    let p1 = reader::rows_since(root.memory(), None, 2).unwrap();
    assert_eq!(p1.len(), 2);
    assert_eq!(p1[0].created_at_ms, 1000);
    assert_eq!(p1[1].created_at_ms, 1001);
    // 第二页 from = (p1 最后一行的 ms, "<tbl>:<id>")
    let last = &p1[1];
    let from = Some((last.created_at_ms, format!("{}:{}", last.key.table.prefix(), last.key.id)));
    let p2 = reader::rows_since(root.memory(), from, 2).unwrap();
    assert_eq!(p2.len(), 2);
    assert_eq!(p2[0].created_at_ms, 1002, "p2 起点必须是 p1 末行的下一条");
    assert_eq!(p2[1].created_at_ms, 1003);
    // 第三页
    let last2 = &p2[1];
    let from2 = Some((last2.created_at_ms, format!("{}:{}", last2.key.table.prefix(), last2.key.id)));
    let p3 = reader::rows_since(root.memory(), from2, 100).unwrap();
    assert_eq!(p3.len(), 1);
    assert_eq!(p3[0].created_at_ms, 1004);
    // 续接不漏不重：p1 + p2 + p3 = 5 条不重复
    let mut all: Vec<String> = p1.iter().chain(p2.iter()).chain(p3.iter())
        .map(|r| format!("{}:{}", r.key.table.prefix(), r.key.id)).collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 5, "分页导致行重复或漏选");
}

#[test]
fn same_ms_rows_break_tie_on_table_then_id() {
    // 同 created_at_ms 多条 ⇒ (tbl, id) 字典序 tie-break；这条是裁定 6 的"全序"承诺面
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    put_card(root.memory(), &card("a", "x"), &prov(), 5000).unwrap();
    put_card(root.memory(), &card("b", "x"), &prov(), 5000).unwrap();
    put_card(root.memory(), &card("c", "x"), &prov(), 5000).unwrap();
    let rows = reader::rows_since(root.memory(), None, 100).unwrap();
    assert_eq!(rows.len(), 3);
    // 都同 ms 5000 ⇒ 按 id 字典序（都是 card 表 ⇒ tbl 相同 ⇒ 比 id）
    let ids: Vec<&str> = rows.iter().map(|r| r.key.id.as_str()).collect();
    let mut sorted_ids = ids.clone(); sorted_ids.sort();
    assert_eq!(ids, sorted_ids, "同 ms 行未按 id 字典序 tie-break");
}

#[test]
fn same_ms_group_pages_on_the_cursor_tie_break() {
    // 批边界落在同 ms 组中间：两行同钟、同一张 card 表。
    // 这一条给两处从未被覆盖的形状上牙：
    //   ① 游标的等值分支 `OR (created_at_ms = ?1 AND (tbl||':'||id) > ?2)`——
    //      整条撤掉 ⇒ 第二页漏第二行；把 `> ?2` 写成 `>= ?2` ⇒ 第二页重吐第一行；
    //   ② `ORDER BY … id ASC`——同表内 id 是唯一 tie-break，游标键是 `tbl || ':' || id`。
    //      canonical 之后 `tbl` 恒为 `card`，组的次序由 id 决定（跨表那一半已无表可跨）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let a = put_card(root.memory(), &card("cx", "x"), &prov(), 7000).unwrap();
    let b = put_card(root.memory(), &card("ex", "x"), &prov(), 7000).unwrap();
    let (lo, hi) = if a.id < b.id { (&a, &b) } else { (&b, &a) };
    let all = reader::rows_since(root.memory(), None, 1_048_576).unwrap();
    assert_eq!(all.len(), 2, "两行同钟都该在活集里");
    let ids: Vec<&str> = all.iter().map(|r| r.key.id.as_str()).collect();
    assert_eq!(ids, vec![lo.id.as_str(), hi.id.as_str()],
               "同 ms 行未按 id 字典序 ⇒ 排序键与游标键分叉");
    // limit=1 逐页取（循环上界 4 ⇒ 重吐型变异不会 runaway）
    let expected: Vec<String> = all.iter()
        .map(|r| format!("{}:{}", r.key.table.prefix(), r.key.id)).collect();
    let mut seen: Vec<String> = Vec::new();
    let mut from = None;
    for _ in 0..4 {
        let page = reader::rows_since(root.memory(), from.clone(), 1).unwrap();
        let Some(last) = page.last() else { break };
        let key = format!("{}:{}", last.key.table.prefix(), last.key.id);
        from = Some((last.created_at_ms, key.clone()));
        seen.push(key);
    }
    assert_eq!(seen, expected, "同 ms 组的批边界：分页序列与全量序分叉（漏选或重吐）");
    // 计数与取行共用同一条等值分支：停在组内第一行之后 ⇒ 两边都该余 1
    let cur = Some((all[0].created_at_ms, expected[0].clone()));
    assert_eq!(reader::count_rows_since(root.memory(), cur.clone()).unwrap(), 1,
               "计数停在同 ms 组中间 ⇒ 与 rows_since 的等值分支分叉");
    assert_eq!(reader::rows_since(root.memory(), cur, 100).unwrap().len(), 1);
}

#[test]
fn dead_rows_are_skipped_and_do_not_consume_the_limit() {
    // 1 死 2 活，limit=2 ⇒ 必须拿到那两条活行（死行不进批、不占行限；裁定 6）。
    // **死行的钟要排在最前**：撤掉活行谓词时它才会挤进 `LIMIT 2` 的窗口，下面三条断言才都咬在同一条守卫上。
    // （死行放最后 ⇒ 撤守卫照样绿，那一形没有牙。）
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k_dead = put_card(root.memory(), &card("a3", "x"), &prov(), 1000).unwrap();
    let k_alive_1 = put_card(root.memory(), &card("a1", "x"), &prov(), 2000).unwrap();
    let k_alive_2 = put_card(root.memory(), &card("a2", "x"), &prov(), 3000).unwrap();
    retire(root.memory(), &k_dead, "synthetic-reason", 9999).unwrap();
    // 死行在表里仍在，但 readers 用 alive_where 应滤掉
    let rows = reader::rows_since(root.memory(), None, 2).unwrap();
    let times: Vec<i64> = rows.iter().map(|r| r.created_at_ms).collect();
    // 这一条同时钉住"取到几条"和"取到哪几条"：撤守卫 ⇒ `[1000, 2000]`；先 LIMIT 后过滤 ⇒ `[2000]`。两种都红。
    assert_eq!(times, vec![2000, 3000], "死行占了行限 ⇒ limit=2 该是两条活行，结果里却混进了钟最早的退场行");
    let ids: Vec<String> = rows.iter().map(|r| r.key.id.clone()).collect();
    assert!(!ids.contains(&k_dead.id), "退场行不应出现在结果中");
    assert!(ids.contains(&k_alive_1.id) && ids.contains(&k_alive_2.id), "两条活行都应在结果中");
}

#[test]
fn count_rows_since_agrees_with_rows_since_from_the_same_watermark() {
    // 同源对拍：`count_rows_since(from)` 必须等于"从同一起点 `rows_since` 能取到的行数"。
    // 两条读法各自拼 SQL，这一条就是它们没有分叉的证据（守卫 3 的牙 + derive_lag_rows 的真生产者）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for i in 0..4 { put_card(root.memory(), &card(&format!("d{i}"), "x"), &prov(), 1000 + i).unwrap(); }
    let dead = put_card(root.memory(), &card("zz", "x"), &prov(), 9000).unwrap();
    retire(root.memory(), &dead, "synthetic-reason", 9999).unwrap();

    // 从头：4 活 + 1 死 ⇒ 两边都只数活的
    let all = reader::rows_since(root.memory(), None, 1_048_576).unwrap();
    assert_eq!(all.len(), 4, "全量取应拿到 4 条活行");
    assert_eq!(reader::count_rows_since(root.memory(), None).unwrap(), all.len() as i64);

    // 游标压在 ms=1001 那条上 ⇒ 两边都该剩 2（1002、1003；死行在 9000 也不补）
    let at = all.iter().find(|r| r.created_at_ms == 1001).unwrap();
    let from = Some((at.created_at_ms, format!("{}:{}", at.key.table.prefix(), at.key.id)));
    let rest = reader::rows_since(root.memory(), from.clone(), 1_048_576).unwrap();
    assert_eq!(rest.len(), 2);
    assert_eq!(reader::count_rows_since(root.memory(), from).unwrap(), rest.len() as i64,
               "计数与取行在同一起点上分叉 ⇒ derive_lag_rows 会与实批打架");

    // 水位推到最末活行 ⇒ 0 余量 ⇔ 取不到行（这一对是"已追平"的读数形状，两侧都得为零）
    let last = rest.last().unwrap();
    let at_end = Some((last.created_at_ms, format!("{}:{}", last.key.table.prefix(), last.key.id)));
    assert_eq!(reader::count_rows_since(root.memory(), at_end.clone()).unwrap(), 0,
               "追平后余量应为零");
    assert!(reader::rows_since(root.memory(), at_end, 1).unwrap().is_empty());
}

// ===== I-1 断根：推导写的行不进下一轮批 =====

/// **走真写入路径**写推导行（`put_timeline_rows` 落 fact 行 + 一条 `ref_kind` 非 `'turn'` 的
/// provenance），不复用本文件那把 `card()` 助手——那把走旧四把 `put_card`，它的 provenance
/// **不写 `ref_kind` 列**（走 DDL 默认 `'turn'`），写出来的正是"抽取行"。
/// 两条用例的差别只有这一点，所以它们必须成对出现：一条说推导行不进批，下一条说抽取行照进。
#[test]
fn derived_row_is_not_reselected_into_the_next_derive_batch() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let extracted = put_card(root.memory(), &card("decision", "ship behind a flag"),
                             &prov(), 1000).unwrap();
    // P 独占列只从 `BatchCtx` 进写入口（守卫 11），这两个字面量与 `rc_derive::derive_pass`
    // 的那两个常量逐字同串（守卫：署名值不许漂）。
    let ctx = BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() };
    let n = put_timeline_rows(root.memory(), &[TimelineContent {
        title: "derive-一行".into(), body: "锚在 ship 上".into(), scope: scope(),
        time_anchor_ms: Some(8_650_000), time_anchor_id: Some(extracted.id.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![],
    }], &ctx, 3000).unwrap();
    assert_eq!(n, 1, "推导时间行确实落了库——这条断的是读侧筛掉它，不是写侧没写");

    let rows = reader::rows_since(root.memory(), None, 100).unwrap();
    assert_eq!(rows.len(), 1, "推导行不进下一轮推导批（它已经是结论，不是依据）");
    assert_eq!(rows[0].key.id, extracted.id, "留在批里的只有那张抽取行");
}

/// 同源对拍：**两侧必须套同一个谓词**。只在 `rows_since` 一侧过滤的话，`count_rows_since`
/// 会继续数推导行 ⇒ agent 面 `derive_lag_rows` 永不归零（"还剩几行没推导"与
/// "下一批实际取到几行"给出相反答案，而这两格同时挂在 status 面上）。
/// 红形（只在一侧过滤）：`count_rows_since` 比 `rows_since` 多 1。
///
/// **推导行必须带锚**：`put_timeline_rows` 的 provenance 是 `if let Some(anchor_id)` 才写的，
/// `time_anchor_id: None` 的行**零出处、零 provenance**，按判别谓词它压根不是"推导写的行"
/// （这正是 T4 那条 `zero_provenance_row…` 断言的形状）。不写锚的话这一条恒绿或恒红都不是在测谓词。
#[test]
fn count_rows_since_and_rows_since_agree_while_a_derived_row_exists() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    put_card(root.memory(), &card("lesson", "正文一"), &prov(), 1000).unwrap();
    let anchor = put_card(root.memory(), &card("lesson", "正文二"), &prov(), 2000).unwrap();
    let ctx = BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() };
    put_timeline_rows(root.memory(), &[TimelineContent {
        title: "derive-一行".into(), body: "正文".into(), scope: scope(),
        time_anchor_ms: Some(9_000), time_anchor_id: Some(anchor.id.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![],
    }], &ctx, 3000).unwrap();

    let all = reader::rows_since(root.memory(), None, 1_048_576).unwrap();
    assert_eq!(reader::count_rows_since(root.memory(), None).unwrap(), all.len() as i64,
        "计数与取批必须同口径——只在一侧过滤的话 derive_lag_rows 永不归零");
    assert_eq!(all.len(), 2, "库里三行，推导行那一行两边都看不见");
}

/// **零出处的推导行同样不进批**——这一条是**判别谓词方向**的判据：谓词若写成
/// `ref_kind <> 'turn'`，无锚推导行（provenance 零行）一条 `ref_kind` 都没有 ⇒ 谓词恒不成立
/// ⇒ 那批行照进批——而它们恰恰是 T4 评审 I-1 描述的**毒化路径入口**
/// （模型对一条零出处行产边 ⇒ `anchor_role_of` 查不到出处）。
/// 与上一条的差别只有 `time_anchor_id`（上一条有锚 ⇒ provenance 一行、ref_kind 非 `'turn'`）。
#[test]
fn zero_provenance_derived_row_is_not_reselected_either() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    put_card(root.memory(), &card("lesson", "抽取来的那一行"), &prov(), 1000).unwrap();
    let ctx = BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() };
    let n = put_timeline_rows(root.memory(), &[TimelineContent {
        title: "derive-无锚".into(), body: "正文".into(), scope: scope(),
        time_anchor_ms: None, time_anchor_id: None, time_confidence: "unknown".into(),
        basis_ids: vec![],
    }], &ctx, 3000).unwrap();
    assert_eq!(n, 1, "无锚推导行照落（裁定 13：没有可重算的锚就不许猜时间）");

    let n_prov: i64 = root.memory().conn().query_row(
        "SELECT COUNT(*) FROM provenance WHERE mem_table='card' AND ref_kind != 'turn'", [], |r| r.get(0)).unwrap();
    assert_eq!(n_prov, 0, "无锚 ⇒ provenance 零行（这一格是判别谓词能覆盖零出处行的前提）");

    let rows = reader::rows_since(root.memory(), None, 100).unwrap();
    assert_eq!(rows.len(), 1, "零出处的推导行也不进下一轮批——它就是 I-1 毒化路径的入口");
}
