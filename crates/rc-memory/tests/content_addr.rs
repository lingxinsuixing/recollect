use rc_memory::{compute_mem_id, keys::MemKey, lookup, put_card, retire, MemTable, Provenance,
                AnchorRole, FactContent, Scope, ScopeKind};
use rc_store::Root;
// scope()/prov()/card() 各用例自带一份（同上，tests 之间不 import）

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() } }
fn prov() -> Vec<Provenance> { vec![Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Human }] }
fn card() -> FactContent {
    FactContent { kind: "decision".into(), title: "先跑测试再合并".into(),
                  body: "红的不许合。".into(), scope: scope(), valid_to_ms: None }
}

#[test]
fn the_id_is_a_function_of_content_only() {
    // 时间、出处、归属时间戳都换了，id 不许变——否则重建换 id，
    // 我写进 vote 或自己笔记里的那个句柄会在重建后锚到别的东西上，而悬空表态比没有表态更坏。
    let s = Scope { kind: ScopeKind::Project, key: "k".into() };
    let a = compute_mem_id(MemTable::Card, "decision", "先跑测试再合并", "红的不许合。", &s);
    let b = compute_mem_id(MemTable::Card, "decision", "先跑测试再合并", "红的不许合。", &s);
    assert_eq!(a, b);
    assert_eq!(a.len(), 64);
    assert_ne!(a, compute_mem_id(MemTable::Card, "decision", "先跑测试再合并", "红的不许合", &s),
              "正文差一个句号就是另一条内容");
    assert_ne!(a, compute_mem_id(MemTable::Anchor, "decision", "先跑测试再合并", "红的不许合。", &s),
              "跨表同内容不许撞 id（preimage 里有表前缀：anchor ≠ card）");
    // A39 那一型的根，钉在 preimage 这一层：**kind 逐字进哈希**，不折叠大小写、不归一。
    // 所以同一件事的两种 kind 写法算出两个句柄 ⇒ 存量库里"同题两行"不会自己合，
    // `duplicate_title_rows` 那一族的读者在 T14 关掉产生源之后仍然在场。
    assert_ne!(a, compute_mem_id(MemTable::Card, "Decision", "先跑测试再合并", "红的不许合。", &s),
              "kind 被折叠过（大小写或归一）⇒ 两种写法会撞成同一个句柄，A39 的存量两行就读不出了");
}

// ── 派生差分：三格各断一次，**直断生产算法** ──────────────────────────────────
//
// 这三条不铺锚行、不跑写入口、不读库里任何一行：它们只把同一个函数换一格入参再比两串 hex。
// 所以它们证明的是"preimage 里到底有没有那一格"，而不是"夹具与实现是不是同一份写法"。

#[test]
fn the_same_name_in_another_scope_is_another_anchor() {
    let a = Scope { kind: ScopeKind::Project, key: "proj-a".into() };
    let b = Scope { kind: ScopeKind::Project, key: "proj-b".into() };
    let x = compute_mem_id(MemTable::Anchor, "person", "同名", "", &a);
    let y = compute_mem_id(MemTable::Anchor, "person", "同名", "", &b);
    assert_ne!(x, y, "同 kind 同 canonical、scope 键不同 ⇒ 必须是两个锚（归属键进了 preimage）");
    // 归属类型那一格同理：同键、只换类型也是另一个锚。
    let c = Scope { kind: ScopeKind::Entity, key: "proj-a".into() };
    assert_ne!(x, compute_mem_id(MemTable::Anchor, "person", "同名", "", &c),
               "同键、只换 scope 类型 ⇒ 必须是两个锚（归属类型进了 preimage）");
}

#[test]
fn the_same_name_in_another_kind_is_another_anchor() {
    let s = scope();
    assert_ne!(compute_mem_id(MemTable::Anchor, "person", "同名", "", &s),
               compute_mem_id(MemTable::Anchor, "work", "同名", "", &s),
               "同 canonical 同 scope、kind 不同 ⇒ 两个锚（kind 逐字进 preimage，不折叠不同一）");
}

#[test]
fn line_ending_and_nfc_variants_of_one_canonical_fold_to_one_anchor() {
    let s = scope();
    let lf = compute_mem_id(MemTable::Anchor, "person", "第一行\n第二行", "", &s);
    let crlf = compute_mem_id(MemTable::Anchor, "person", "第一行\r\n第二行", "", &s);
    assert_eq!(lf, crlf, "同一行的 CRLF 与 LF 两种写法必须折成同一个锚");
    // 预组合 é (U+00E9) 与 e + 组合重音 (U+0065 U+0301) 是同一个名字的两种写法。
    let composed = compute_mem_id(MemTable::Anchor, "person", "caf\u{e9}", "", &s);
    let decomposed = compute_mem_id(MemTable::Anchor, "person", "cafe\u{301}", "", &s);
    assert_eq!(composed, decomposed, "同一名字的 NFC 与分解形必须折成同一个锚");
}

#[test]
fn re_ingesting_the_same_content_does_not_add_a_row_but_does_add_provenance() {
    // G2 的记忆层那一半：同一会话的第 N 次导出不许在库里长出第二份。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k1 = put_card(root.memory(), &card(), &prov(), 1000).unwrap();
    let mut p2 = prov();
    p2[0] = Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Agent };
    let k2 = put_card(root.memory(), &card(), &p2, 2000).unwrap();
    assert_eq!(k1, k2, "内容相同 ⇒ 同一个句柄，第二次是补出处不是新记忆");
    let n: i64 = root.memory().conn().query_row("SELECT count(*) FROM card", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 1);
    let n: i64 = root.memory().conn()
        .query_row("SELECT count(*) FROM provenance WHERE mem_id=?1", [&k1.id], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "出处是一张集合，两条都要在");
    // 强锚取"任一出处是人工轮"⇒ 补一条 agent 轮不许把强锚洗掉（V6 的载体）
    assert_eq!(lookup(root.memory(), &k1).unwrap().unwrap().anchor_role, AnchorRole::Human);
}

#[test]
fn a_gone_row_answers_absent_rather_than_nearest_neighbour() {
    // §6 连带②：指向已消失内容的句柄必须显式查不到。静默返回最近邻会让我以为自己确认过某件事。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    put_card(root.memory(), &card(), &prov(), 1).unwrap();
    for id in ["f".repeat(64), "e".repeat(64)] {
        let ghost = MemKey { table: MemTable::Card, id };
        assert!(lookup(root.memory(), &ghost).unwrap().is_none(), "前缀相近也不许命中另一条");
    }
    assert_eq!(rc_memory::all_titles(root.memory()).unwrap().len(), 1);
}

#[test]
fn suppression_keys_on_content_not_on_a_row_key() {
    // 守卫 13 的写侧那一半在本阶段可测的形态：账按 (mem_table, mem_id)，
    // 而 mem_id 是内容的函数 ⇒ 重建同一内容仍然是同一条，账仍然命中。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = put_card(root.memory(), &card(), &prov(), 1).unwrap();
    retire(root.memory(), &k, "判为无价值", 2).unwrap();
    let f = lookup(root.memory(), &k).unwrap().unwrap();
    assert!(f.retired);
    assert_eq!(f.title, "先跑测试再合并", "退场不是删除：行还在，只是不许被检索到");
    // 同内容再写一次 ⇒ 同一个 id ⇒ 抑制仍然生效（这条是 §1 派生重建不碰判定族的第一用例）
    let k2 = put_card(root.memory(), &card(), &prov(), 3000).unwrap();
    assert_eq!(k, k2);
    assert!(lookup(root.memory(), &k2).unwrap().unwrap().retired,
            "重写入把抑制状态洗掉了 ⇒ 已退场的记忆复活，且没有任何一处会报错");
}

#[test]
fn 重复播种在两张表上都不增行而出处也不翻倍() {
    // 内容寻址在 DB 那一半有**两条**独立的去重子句：card 走 `card_sql`，
    // anchor 用的是 `writers_x.rs` 里另一段 SQL 字面量。上面几条只把 card 重放过两遍，
    // 所以 anchor 那条 `ON CONFLICT(id) DO NOTHING` 今天必须被单独挣出来。
    // `seed_at` 与 `seed` 内容完全一致、只换 P 时间戳（时钟不进 preimage）⇒ 这就是它存在的理由。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let first = rc_memory::synth::seed(root.memory(), 1_000).unwrap();
    let second = rc_memory::synth::seed_at(root.memory(), 2_000).unwrap();
    assert_eq!(first, second, "同内容 ⇒ 同句柄：第二次播种换了柄就等于让重建把笔记里的句柄冲掉了");

    // 两张表逐个点名（只数总数会被"一张多一行、另一张少一行"抵消而假绿）。
    let count = |t: &str| -> i64 {
        root.memory().conn()
            .query_row(&format!("SELECT count(*) FROM {t}"), [], |r| r.get(0)).unwrap()
    };
    assert_eq!(count("anchor"), 2, "anchor 分支的去重子句没起作用：重复播种长出了第二份锚");
    assert_eq!(count("card"), 5, "card（3 张同分卡 + 1 断言 + 1 事件）被重复播种加了行");
    // 去重发生在 INSERT 层 ⇒ 回执也要看得见：出处与别名表同样一行都不许多。
    assert_eq!(count("provenance"), 7, "出处翻倍：每次重建都给同一条记忆多塞一条同 uid 的出处");
    assert_eq!(count("alias"), 2, "别名表翻倍：同一条锚的同一个别名长了第二行");

    // 再按句柄逐个查一遍出处/别名（表级总数对不上单个句柄时的漏网形状）。
    for k in second.iter().filter(|k| k.table == MemTable::Anchor) {
        let n: i64 = root.memory().conn().query_row(
            "SELECT count(*) FROM provenance WHERE mem_table='anchor' AND mem_id=?1",
            [&k.id], |r| r.get(0)).unwrap();
        assert_eq!(n, 1, "{} 的出处被重复播种加了行", k.key_string());
        let a: i64 = root.memory().conn()
            .query_row("SELECT count(*) FROM alias WHERE anchor_id=?1", [&k.id], |r| r.get(0)).unwrap();
        assert_eq!(a, 1, "{} 的别名被重复播种加了行", k.key_string());
    }
}

// 走模块路径而不是根 re-export：`identity` 本就是 `pub mod`，而本阶段库外只有测试用这把尺子——
// 把 `lib.rs` 的 `pub use identity::compute_mem_id;` 扩成两个名字会造出一个零读者的公开面
// （与 T4 删 `put_provenance_with_ref_kind` 同一条理由）。
use rc_memory::identity::compute_edge_id;

#[test]
fn edge_id_is_a_function_of_endpoints_and_kind_only() {
    // 同 (from, kind, to) 重写幂等；理由变化不算新边（签名里没有 reason 形参 = 编译期执法）。
    let a = compute_edge_id("from-x", "part_of", "to-y");
    let b = compute_edge_id("from-x", "part_of", "to-y");
    assert_eq!(a, b);
    assert_eq!(a.len(), 64);
    // 换 to ⇒ id 变（两个方向各一条）
    assert_ne!(a, compute_edge_id("from-x", "part_of", "to-z"),
               "换 to 端必须算出不同的边 id");
    assert_ne!(a, compute_edge_id("from-z", "part_of", "to-y"),
               "换 from 端必须算出不同的边 id");
    // **0u8 分隔的真牙**（撤分隔的变异必须能为它红）：三段拼接时把边界左移一格，无分隔则两种切法
    // 拼成同一串——("a"|"bc"|"d") 与 ("ab"|"c"|"d") 都拼成 abcd；("x"|"ab"|"c") 与 ("x"|"a"|"bc") 都拼成 xabc。
    assert_ne!(compute_edge_id("a", "bc", "d"), compute_edge_id("ab", "c", "d"),
               "from|kind 边界左移一格必须算出不同的边 id");
    assert_ne!(compute_edge_id("x", "ab", "c"), compute_edge_id("x", "a", "bc"),
               "kind|to 边界左移一格必须算出不同的边 id");
    // kind 变化 ⇒ id 变
    assert_ne!(compute_edge_id("x", "part_of", "y"), compute_edge_id("x", "depends_on", "y"));
}
