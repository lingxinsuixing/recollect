//! `put_batch` 是 §7"缺一整批丢弃"在写入侧的兑现，所以它的三条用例都断**行数**，
//! 不断"有没有报错"。`Err` 但写进去半批，正是这一层唯一不可接受的行为。
use rc_memory::{put_batch, BatchCtx, MemError, MemKey, MemTable, Provenance, Scope, ScopeKind, WriteItem};
use rc_store::Root;

fn ctx() -> BatchCtx {
    // 与 `BatchCtx::synthetic()` 不同的一对值（坑 P-29）：如果实现偷偷继续用合成常量，
    // 下面 `p_columns_come_from_the_ctx` 那条会红，而同一个值测不出这件事。
    BatchCtx { source_agent: "agent-batch-A".into(), device_id: "dev-batch-A".into() }
}

fn item(table: MemTable, title: &str, uid: &str) -> WriteItem {
    WriteItem {
        // anchor.kind 的 CHECK 没有 "lesson"：锚行用域内的 "concept"。
        table,
        kind: (if table == MemTable::Anchor { "concept" } else { "lesson" }).into(),
        title: title.into(), body: "合成正文".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-A".into() },
        valid_to_ms: None, aliases: vec![],
        prov: vec![Provenance { turn_uid: uid.into(), anchor_role: rc_memory::AnchorRole::Human }],
    }
}

fn count(m: &rc_store::MemoryHandle, table: &str) -> i64 {
    m.conn().query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0)).unwrap()
}

#[test]
fn one_bad_title_writes_no_rows_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let items = vec![item(MemTable::Card, "第一条合法", &"a".repeat(64)),
                     item(MemTable::Card, "我", &"b".repeat(64))];      // 守卫 10：代词标题
    let e = put_batch(root.memory(), &items, &ctx(), 1_000).unwrap_err();
    assert!(matches!(e, MemError::AliasIsPronoun(_)), "{e}");
    assert_eq!(count(root.memory(), "card"), 0, "半批落地 = 整批丢弃这条判据失守");
    assert_eq!(count(root.memory(), "provenance"), 0);
}

#[test]
fn p_columns_come_from_the_ctx_not_from_a_constant() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let items = vec![item(MemTable::Card, "先看进度", &"c".repeat(64))];
    let keys = put_batch(root.memory(), &items, &ctx(), 1_000).unwrap();
    assert_eq!(keys.len(), 1, "账本与 items 逐格对齐");
    assert!(keys[0].is_some(), "没被抑制的那一格必须是 Some");
    let (sa, dev): (String, String) = root.memory().conn()
        .query_row("SELECT source_agent, device_id FROM card LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!((sa.as_str(), dev.as_str()), ("agent-batch-A", "dev-batch-A"),
               "P 独占列必须由批次上下文供给（归属表：这两列是 Program 的）");
}

#[test]
fn empty_provenance_rejects_the_whole_batch_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let mut bad = item(MemTable::Card, "合法标题", &"d".repeat(64));
    bad.prov.clear();
    let good = item(MemTable::Anchor, "也合法", &"e".repeat(64));
    let e = put_batch(root.memory(), &[good, bad], &ctx(), 1_000).unwrap_err();
    assert!(matches!(e, MemError::ProvenanceMissing(_)), "{e}");
    assert_eq!(count(root.memory(), "card"), 0);
    assert_eq!(count(root.memory(), "anchor"), 0, "校验发生在任何一条写入之前，不是遇到才停");
}

#[test]
fn a_retired_row_gains_no_new_provenance_and_is_counted_as_suppressed() {
    // 守卫 13"退场不许复活"在本层的实际形状。`compute_mem_id` 是内容寻址 + 写语句是
    // `ON CONFLICT(id) DO NOTHING`，所以重蒸改不动 verdict / superseded_by / retirement —
    // 这一半已经有牙，不是这条用例新加的。**缺的那一半是出处**：换一个 turn_uid 就会
    // `INSERT OR IGNORE` 进一条新 provenance，而 reader 的 `anchor_role` 是从 provenance 算的
    // ⇒ 一条被中心判死的行在 agent 面读起来像"有新证据支持它"。写前查账拦的就是这一笔。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let first = vec![item(MemTable::Card, "先看进度", &"a".repeat(64)),
                     item(MemTable::Anchor, "合成锚", &"f".repeat(64))];
    let keys = put_batch(root.memory(), &first, &ctx(), 1_000).unwrap();
    assert_eq!(keys.len(), 2, "账本逐格对齐：两条都是 Some");
    let live: Vec<MemKey> = keys.iter().flatten().cloned().collect();
    assert_eq!(live.len(), 2);
    rc_memory::retire(root.memory(), &live[0], "audit:vote1", 2_000).unwrap();

    // 同一份内容（id 因此相同）、换一个 uid：不查账的话 provenance 会多出第二条。
    let again = vec![item(MemTable::Card, "先看进度", &"b".repeat(64)),
                     item(MemTable::Anchor, "合成锚", &"g".repeat(64))];
    let got = put_batch(root.memory(), &again, &ctx(), 3_000).unwrap();
    assert_eq!(got.len(), 2, "被抑制的那条也要占一格，不然调用方的 zip 会静默错位");
    assert!(got[0].is_none(), "命中抑制账 ⇒ 这一格是 None");
    assert_eq!(got[1], Some(live[1].clone()), "未命中的那条照写，句柄与第一批逐字节相同（内容寻址）");
    assert_eq!(count(root.memory(), "card"), 1, "原行不许被重写、也不许长第二条");
    assert_eq!(count(root.memory(), "provenance"), 3,
               "两条原始出处 + 未命中那条换 uid 后新增的一条；命中那条若也写就是第 4 条 = 复活");
    assert_eq!(count(root.memory(), "retirement"), 1);
}

#[test]
fn a_ghost_ledger_suppresses_the_write_though_no_row_exists() {
    // 守卫 13 的"有账无行"那一半。`retire` 与 `attest` 都只校验自己那一格（前者插账、后者对
    // 免锚的 `rejected` 连锚都不查），两者都不校验行存在；而 `compute_mem_id` 是公开的内容寻址
    // ⇒ "一张账、零行"是公共 API 走得到的形状（`reader::dangling_attestations` 观察的就是它）。
    // 写前查账若按行筛（数 `count(*) FROM card WHERE id=?1 AND NOT (alive_where)`），这张账查不到，
    // 于是同一份内容下一次照样落库、把中心刚判死的东西复活——三种死法里最安静的一种。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-A".into() };
    let ghost = |table: MemTable, title: &str| {
        let kind = if table == MemTable::Anchor { "concept" } else { "lesson" };
        MemKey { table, id: rc_memory::compute_mem_id(table, kind, title, "合成正文", &scope) }
    };
    rc_memory::retire(root.memory(), &ghost(MemTable::Card, "幽灵退场"), "audit:ghost", 2_000).unwrap();
    rc_memory::attest(root.memory(), &ghost(MemTable::Anchor, "幽灵否决"),
                      rc_memory::Stance::Rejected, None, Some("audit:ghost"), 2_100).unwrap();
    assert_eq!(count(root.memory(), "card"), 0, "前提：只有账，没有行");
    assert_eq!(count(root.memory(), "anchor"), 0, "同上");
    assert_eq!(count(root.memory(), "retirement"), 1, "前提：那张幽灵退场账确实落下了");
    assert_eq!(count(root.memory(), "attestation"), 1, "前提：那张幽灵否决账确实落下了（免锚那条支）");

    let batch = vec![item(MemTable::Card, "幽灵退场", &"a".repeat(64)),
                     item(MemTable::Anchor, "幽灵否决", &"b".repeat(64)),
                     item(MemTable::Card, "无账的那条", &"c".repeat(64)),
                     item(MemTable::Anchor, "锚也走一遍查账", &"d".repeat(64))];
    let got = put_batch(root.memory(), &batch, &ctx(), 3_000).unwrap();
    assert_eq!(got.len(), 4, "抑制的两条各占一格");
    assert!(got[0].is_none(), "未解决的退场账（有账无行）⇒ 抑制");
    assert!(got[1].is_none(), "未解决的否决账（有账无行）⇒ 抑制");
    // 对照组同时钉住上面那个 `ghost` 的算法：kind / 正文 / 归属必须与 `item()` 里那三格一致。
    // 漂了的话前两格会拿到另一个 id ⇒ 查不到账 ⇒ 照写，这条用例就自己失效了。
    assert_eq!(got[2].as_ref().unwrap().id,
               rc_memory::compute_mem_id(MemTable::Card, "lesson", "无账的那条", "合成正文", &scope),
               "没有账的那条照写，且落的正是内容寻址的那一个 id");
    // 第四条是 `anchor` 那一支 `1=1` 分岔在本层唯一的写入侧用例。
    // 它管的不是"新写的锚"（写前查账时行还不存在，第三个 EXISTS 恒假，什么分岔都测不出），
    // 而是**已存在的锚重蒸一次**：正确形状下 `NOT (1=1)` 恒假 ⇒ 照写（`Some`，内容寻址同一个 id）；
    // 有人把分岔写成 `1=0` ⇒ `NOT (1=0)` 恒真 ⇒ 每条已存在的锚都被自己顶掉的那一支抑制掉。
    // 所以这一格要拿"第二条批次的同内容锚"才有牙，第一批那条新写的没有。
    assert_eq!(got[3].as_ref().map(|k| k.table), Some(MemTable::Anchor),
               "锚没有 superseded_by 列，谓词仍要拼得出去且不许因此抑制写入");
    let again = vec![item(MemTable::Anchor, "锚也走一遍查账", &"e".repeat(64))];
    let got2 = put_batch(root.memory(), &again, &ctx(), 3_500).unwrap();
    assert_eq!(got2[0], got[3], "已存在的锚重蒸同一份内容 ⇒ 照写同一个句柄（`1=1` 那支不许变成恒真）");
    assert_eq!(count(root.memory(), "card"), 1, "只有无账那条落了行");
    assert_eq!(count(root.memory(), "anchor"), 1, "否决账指向的幽灵锚不许因为这次写入而长出来（唯一锚是合法那条）");
    assert_eq!(count(root.memory(), "anchor"), 1);
    assert_eq!(count(root.memory(), "provenance"), 3,
               "抑制的两条一条出处都不许追加；写入的两条各一条，再加锚重蒸那一次换 uid 多出的一条");
}

#[test]
fn a_resolved_ghost_ledger_stops_suppressing_the_write() {
    // 上一条的对照半边，也是"`dead_by_id_expr` 选 fail-closed 而不是死路"那句的凭据：
    // `resolve_retirement` 按 `(mem_table, mem_id)` 解、与行在不在无关 ⇒ 一张幽灵账同样解得掉，
    // 解开以后同一份内容照写。哪一天解除路径改成只认已有的行，这条会红。
    // （否决账那一支本阶段**没有**解除路径：`attestation.resolved_at_ms` 全树无人写。
    //   这件事写在 `reader::dead_by_id_expr` 的注释里，不在这里测——测不到的东西不造用例。）
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-A".into() };
    let ghost = MemKey { table: MemTable::Card,
                         id: rc_memory::compute_mem_id(MemTable::Card, "lesson", "解得开的账", "合成正文", &scope) };
    rc_memory::retire(root.memory(), &ghost, "audit:ghost", 2_000).unwrap();
    let suppressed = put_batch(root.memory(), &[item(MemTable::Card, "解得开的账", &"a".repeat(64))],
                               &ctx(), 3_000).unwrap();
    assert!(suppressed[0].is_none(), "先确认这张幽灵账此刻确实在抑制");

    rc_memory::resolve_retirement(root.memory(), &ghost, 4_000).unwrap();
    let got = put_batch(root.memory(), &[item(MemTable::Card, "解得开的账", &"b".repeat(64))],
                       &ctx(), 5_000).unwrap();
    assert_eq!(got[0], Some(ghost.clone()), "账解开 ⇒ 同一份内容照写，落的正是那一个内容寻址 id");
    assert_eq!(count(root.memory(), "card"), 1);
    assert_eq!(count(root.memory(), "provenance"), 1, "抑制那一次一条出处都没写，所以这里只有第二条 uid 那一条");
    assert_eq!(count(root.memory(), "retirement"), 1, "解除是把同一张账标记为已解，不长第二条");
}
