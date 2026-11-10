//! 批次③ 段 C-1 · 因果链**读侧**的用例（R1–R4）。
//!
//! 段 B 把 `put_chains` 接进了生产链路，但今天没有任何读者能把链条读回来。本文件钉的就是
//! 那一格**第一次可见**的东西：`chain_links` 按 `ord` 读环节、`provenance_chain` 不再把环节当依据、
//! 链头不被当未审核事实回喂。库面断言（P1–P8）已在 `put_chains.rs` 落死，本文件与它的交点只是
//! **同一个夹具形状**（三张 human 出处的种子卡）——那里钉写入，这里钉读出，两格不互相顶。

use rc_memory::reader::{self, chain_links, provenance_chain};
use rc_memory::writers_x::{put_card, put_chains, BatchCtx, ChainContent, FactContent};
use rc_memory::{AnchorRole, ChainLink, MemKey, MemTable, Provenance, ProvenanceStep, Scope, ScopeKind};
use rc_store::MemoryHandle;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-chain".into() } }

/// 与 `rc_derive::derive_pass` 那两个署名常量逐字同串（守卫 11：署名值只从 `BatchCtx` 进写入口）。
fn ctx() -> BatchCtx { BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() } }

/// 抽取侧形状的出处（`ref_kind` 走 DDL 默认 `'turn'`、`role` 走 DEFAULT `'basis'`）⇒ 种子卡自己是
/// 「抽取写的行」，`anchor_role_of` 读回来的最强锚因此是 Human。链头引它 ⇒ `chain_links` 的
/// `anchor_role` 读出 Human，`ref_kind` 读出 `card`。
fn prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Human }]
}

fn card(title: &str, body: &str) -> FactContent {
    FactContent { kind: "decision".into(), title: title.into(), body: body.into(),
                  scope: scope(), valid_to_ms: None }
}

/// 三张真存在的 card：`put_chains` 的环节与依据都必须是四张事实表里的行，否则 `table_of` 当场报错。
/// 与 `put_chains.rs::seed3` 同形（R1/R2 用同一个夹具形状，但各写各的、互不引用）。
fn seed3(m: &MemoryHandle) -> [String; 3] {
    let a = put_card(m, &card("环节甲", "甲这一行的正文"), &prov(), 1000).unwrap().id;
    let b = put_card(m, &card("环节乙", "乙这一行的正文"), &prov(), 2000).unwrap().id;
    let c = put_card(m, &card("依据丙", "丙这一行的正文"), &prov(), 3000).unwrap().id;
    [a, b, c]
}

fn chain(mechanism: &str, link_ids: Vec<String>, basis_ids: Vec<String>) -> ChainContent {
    ChainContent { mechanism: mechanism.into(), link_ids, basis_ids,
                   causal_type: "causes".into(), scope: scope() }
}

/// 链头句柄：`event` 表 + 那条链的头行 id。
fn head_key(id: &str) -> MemKey { MemKey { table: MemTable::Card, id: id.to_string() } }

/// 按机制句取链头 id。C-2 让链头的 `body` 就是机制句全文（写侧不 normalize body 那列），
/// 所以 `body=<机制句>` 唯一定位到那一条链的头行——不靠 `compute_chain_id` 把哈希方案也钉进用例。
fn head_id(m: &MemoryHandle, mechanism: &str) -> String {
    m.conn().query_row(
        "SELECT id FROM card WHERE kind='causal' AND text=?1", [mechanism],
        |r| r.get::<_, String>(0)).unwrap()
}

fn target_ids(links: &[ChainLink]) -> Vec<String> {
    links.iter().map(|l| l.target_id.clone()).collect()
}

/// R1（工单 §5-1 第一条 · `chain_links` 按 `ord` 还原方向）：`A→B` 与 `B→A` 两条链各自读回自己那一条。
/// 变异形（本条实测过）：把 `chain_links` 那句 `ORDER BY ord` 换成 `ORDER BY turn_uid`
///（照抄既有 `provenance_chain` 的排序，是最像的假形状）⇒ 两条链都按句柄字典序返回**同一个**序列。
/// a≠b ⇒ 两头的 ord 序（[甲,乙] 与 [乙,甲]）与 turn_uid 序不可能同时成立 ⇒ 至少一个反向断言红。
#[test]
fn 链头按_ord_读回环节_两条反向的链各得自己那条序() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let (a, b) = (ids[0].clone(), ids[1].clone());
    put_chains(m, &[chain("正向的机制句", vec![a.clone(), b.clone()], vec![]),
                    chain("反向的机制句", vec![b.clone(), a.clone()], vec![])],
               &ctx(), NOW).unwrap();
    let l1 = chain_links(m, &head_key(&head_id(m, "正向的机制句"))).unwrap();
    let l2 = chain_links(m, &head_key(&head_id(m, "反向的机制句"))).unwrap();
    assert_eq!(target_ids(&l1), vec![a.clone(), b.clone()],
        "A→B 那条必须按写入序读回 [A, B]（顺序读得回来是本批唯一的价值）");
    assert_eq!(target_ids(&l2), vec![b.clone(), a.clone()],
        "B→A 那条必须读回 [B, A]；两条同序就是把方向抹平了");
    // 三格今天必然有值：ref_kind = 被引行所在表前缀，anchor_role = 被引行的最强锚（种子卡 human 出处）。
    for link in l1.iter().chain(l2.iter()) {
        assert_eq!(link.ref_kind, "card", "环节引的是 card ⇒ ref_kind 必须是那张表的前缀");
        assert_eq!(link.anchor_role, AnchorRole::Human,
            "被引行的最强锚是 human ⇒ anchor_role 跟着来源走");
    }
}

/// R2（工单 §5-1 第一条 · `chain_links` 只给环节）：`role='basis'` 的依据行不在结果里。
/// 夹具是 `basis ⊆ link` 那一形（甲既是环节又是依据 ⇒ 库里只留 link 那一行，丙是纯依据）。
/// 变异形（本条实测过）：去掉 `chain_links` 里 `role='link'` 那道筛 ⇒ 在这个夹具上环节数变多
///（丙以 `role='basis'` 也进结果，`ord` 为 NULL 排到最前 ⇒ 返回 3 行）⇒ R2 红。
#[test]
fn chain_links_只给环节_依据行不混进来() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let (a, b, c) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
    put_chains(m, &[chain("甲先于乙丙凭什么", vec![a.clone(), b.clone()],
                          vec![a.clone(), c.clone()])], &ctx(), NOW).unwrap();
    let links = chain_links(m, &head_key(&head_id(m, "甲先于乙丙凭什么"))).unwrap();
    assert_eq!(links.len(), 2, "环节只有甲、乙两个：丙是纯依据（role='basis'），不许混进环节");
    assert_eq!(target_ids(&links), vec![a.clone(), b.clone()]);
    assert!(!target_ids(&links).contains(&c),
        "丙只是依据不是环节 ⇒ 不该被 chain_links 读回来");
}

/// R3（工单 §5-1 第二条 · `provenance_chain` 不再把环节当依据）：同一链头的 `Basis` 步数 = 库里依据行数。
/// 夹具 `basis=[丙]` 与 `link=[甲,乙]` 不相交 ⇒ 库里 `role='basis'` 的行恰一行（丙）。
/// 变异形（本条实测过）：把 role 筛那一句 `if role == "link" { continue; }` 删掉（跳过筛）⇒
/// 两个环节甲、乙也被当成 Basis 报出去 ⇒ Basis 步数从 1 抬到 3 ⇒ 本条红。
#[test]
fn provenance_chain_把环节和依据分家_环节不进basis步() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let (a, b, c) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
    put_chains(m, &[chain("环节甲乙凭依据丙", vec![a.clone(), b.clone()], vec![c.clone()])],
               &ctx(), NOW).unwrap();
    let steps = provenance_chain(m, &head_key(&head_id(m, "环节甲乙凭依据丙"))).unwrap();
    let basis_n = steps.iter().filter(|s| matches!(s, ProvenanceStep::Basis { .. })).count();
    // basis_ids 给了一个（丙），库里 role='basis' 的行也是一个 ⇒ 二者对上；含环节就是 3 而非 1。
    assert_eq!(basis_n, 1, "环节不当依据 ⇒ Basis 步数只数真依据（丙）。把甲乙也算进来就是 {basis_n}");
    // 链头没有任何 ref_kind='turn' 出处 ⇒ 不渲直接轮次（那一支一字未动，仍走 ref_kind=='turn'）。
    assert!(steps.iter().all(|s| !matches!(s, ProvenanceStep::Turn { .. })),
        "链头没有 turn 出处 ⇒ 不该有直接轮次那一步：{steps:?}");
    // 三个变体的形状要保住：依据在场 ⇒ 它自己的出处轮次跟着渲成 BasisTurn（丙是 human 出处的卡）。
    assert!(steps.iter().any(|s| matches!(s, ProvenanceStep::BasisTurn { .. })),
        "依据丙在场、它自己有一条 turn 出处 ⇒ BasisTurn 那一支必须还在：{steps:?}");
}

/// R4（工单 §5-1 第一条与 `turn_basis_pred` 的交点 · 零回灌）：链头没有 `ref_kind='turn'` 的出处 ⇒
/// 不进 `unreviewed_rows`、不进推导池。这一格钉的是「链头不该被当未审核事实回喂」。
/// 变异形（本条实测过 = 下面那段负控制**真造了一次**）：给链头补一行 `ref_kind='turn'` 的出处 ⇒
/// 它被 `turn_basis_pred` 认成「抽取写的行」⇒ 前半的「不在」当场翻成「在」。所以这一条自带牙。
#[test]
fn 链头没有turn出处就不进游标也不进推导池() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    put_chains(m, &[chain("甲顶掉乙", vec![ids[0].clone(), ids[1].clone()], vec![])],
               &ctx(), NOW).unwrap();
    let h = head_id(m, "甲顶掉乙");
    let head_in_cursor = |mm: &MemoryHandle| -> bool {
        reader::unreviewed_rows(mm, 100).unwrap().into_iter().any(|r| r.key.id == h)
    };
    let head_in_pool = |mm: &MemoryHandle| -> bool {
        reader::rows_since(mm, None, 100).unwrap().into_iter().any(|r| r.key.id == h)
    };
    assert!(!head_in_cursor(m), "链头是别的 pass 的结论，不该再花一次判定额度");
    assert!(!head_in_pool(m), "链头不回喂下一轮推导批（零回灌那一格）");

    // 负控制 = 变异形本身，真造一次：把链头的一行 turn 出处插进库里。
    // id 与 turn_uid 都是 hex / 重复字符 ⇒ 拼进 SQL 没有引号风险（同 export_doc.rs 那几处 execute_batch）。
    let turn_uid = "x".repeat(64);
    m.conn().execute_batch(&format!(
        "INSERT INTO provenance(mem_table, mem_id, turn_uid, anchor_role, ref_kind) \
         VALUES('card', '{}', '{}', 'human', 'turn')", h, turn_uid)).unwrap();
    assert!(head_in_cursor(m),
        "补了 turn 出处的链头应当被认成抽取行而进游标——否则前半那句「不在」根本没牙（谓词没在筛）");
    assert!(head_in_pool(m),
        "补了 turn 出处的链头应当回喂推导池；不进就是谓词压根没生效");
}
