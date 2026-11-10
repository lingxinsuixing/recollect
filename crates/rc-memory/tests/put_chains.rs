//! 批次③ 段 B-2 · 因果链写入口 `put_chains` 的用例（P1–P8）。
//!
//! 这一族的可观察性只有两格：`event(kind='causal')` 的头行与 `provenance` 里 `role`/`ord`
//! 两列。读侧（`chain_links`）归段 C，所以本文件一律按库面断言：能进用例的形状都从
//! `put_chains` 走真写入路径进来，不手搓 `event`/`provenance` 行（手搓等于把被测的那两句
//! SQL 换成用例自己的 SQL，生产代码改坏也不会红）。

use rc_memory::keys::PROVENANCE_ROLES;
use rc_memory::reader;
use rc_memory::writers_x::{put_card, put_chains, put_chains_merged, BatchCtx, ChainContent,
                           FactContent};
use rc_memory::{AnchorRole, MemError, Provenance, Scope, ScopeKind};
use rc_store::MemoryHandle;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-chain".into() } }

/// P 独占列只从 `BatchCtx` 进写入口（守卫 11）。这两个字面量与 `rc_derive::derive_pass`
/// 的那两个常量逐字同串，用例这里跟着同一条：署名值不许漂。
fn ctx() -> BatchCtx { BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() } }

/// 抽取侧形状的出处（`ref_kind` 走 DDL 默认 `'turn'`）⇒ 种子行自己是「抽取写的行」，
/// 与后面链头那批 `ref_kind` 非 `'turn'` 的出处行正好分成两侧（P7 要用这个对照）。
fn prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Human }]
}

fn card(title: &str, body: &str) -> FactContent {
    FactContent { kind: "decision".into(), title: title.into(), body: body.into(),
                  scope: scope(), valid_to_ms: None }
}

/// 三张真存在的 card：`put_chains` 的环节与依据都必须是四张事实表里的行，
/// 否则 `writers_x::table_of` 当场 `Sql("derived basis id is in no fact table")`——
/// 那是另一条形状（本文件不测它，段 C 也不该把它写成因果链的牙）。
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

fn scalar_i64(m: &MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// 链头行的 id 集合。`ORDER BY id`：hex 句柄的序与 SQLite 的 BINARY collation 逐字符一致，
/// 断言因此不依赖插入顺序。
fn causal_heads(m: &MemoryHandle) -> Vec<String> {
    let mut st = m.conn().prepare("SELECT id FROM card WHERE kind='causal' ORDER BY id").unwrap();
    st.query_map([], |r| r.get::<_, String>(0)).unwrap().map(|x| x.unwrap()).collect()
}

/// 一条链头的出处行（`turn_uid`、`role`、`ord`、`ref_kind`）。`ord` 是可空列 ⇒ `Option<i64>`。
fn prov_rows(m: &MemoryHandle, head: &str) -> Vec<(String, String, Option<i64>, String)> {
    let mut st = m.conn().prepare(
        "SELECT turn_uid, role, ord, ref_kind FROM provenance
         WHERE mem_table='card' AND mem_id=?1 ORDER BY role, ord, turn_uid").unwrap();
    st.query_map([head], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap().map(|x| x.unwrap()).collect()
}

/// 一条链头的环节序列：`(ord, turn_uid)` 按 `ord` 升序。
/// 这一支就是日后段 C 的 `chain_links` 要读的东西，本段先按库面把它钉住。
fn link_seq(m: &MemoryHandle, head: &str) -> Vec<(i64, String)> {
    let mut st = m.conn().prepare(
        "SELECT ord, turn_uid FROM provenance
         WHERE mem_table='card' AND mem_id=?1 AND role='link' ORDER BY ord").unwrap();
    st.query_map([head], |r| Ok((r.get::<_, i64>(0)?, r.get(1)?)))
        .unwrap().map(|x| x.unwrap()).collect()
}

/// P1（工单 C-1）：**同机制、不同环节 ⇒ 库里两行**。
/// 变异形（本条实测过）：把 `put_chains` 头行那句 `compute_chain_id` 换成 `compute_mem_id` ⇒
/// 两条链的 preimage 只剩（表、kind、title、body、scope），而 title/body 都由同一句机制决定
/// ⇒ 同一个 id，第二条被头行那句 `INSERT OR IGNORE` 静默吞掉：本条红在返回条数那句（读数 1），
/// 链头数与环节总数是同一发变异的后续后果（红只报第一个未通过的断言）。
#[test]
fn 同机制不同环节的两条链各自落一个头行() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let mech = "先立的决定把后面的排期顶掉了";
    let rows = vec![
        chain(mech, vec![ids[0].clone(), ids[1].clone()], vec![]),
        chain(mech, vec![ids[0].clone(), ids[2].clone()], vec![]),
    ];
    let n = put_chains(m, &rows, &ctx(), NOW).unwrap();
    assert_eq!(n, 2, "两条链 = 返回 2（返回的是链条数，环节行数之和是 4）");
    let heads = causal_heads(m);
    assert_eq!(heads.len(), 2, "同机制、不同环节必须落两个头行 ⇒ 只落一个就是环节被吞了");
    let links = scalar_i64(m, "SELECT count(*) FROM provenance WHERE role='link'");
    assert_eq!(links, 4, "两个头行各自的环节都得在库里（第二条链的两个环节一条都不能少）");
    assert_eq!(scalar_i64(m, "SELECT count(*) FROM card WHERE kind='causal'"), 2);
    // 两个头行的正文都是那句机制本身（C-2），差别只在环节 ⇒ 正文不能当区分依据（正是 C-1 的洞）。
    assert_eq!(scalar_i64(m, "SELECT count(DISTINCT text) FROM card WHERE kind='causal'"), 1,
        "夹具前提破了：两条链的机制句必须逐字相同，否则本条钉不出 C-1 那次碰撞");
}

/// P2（验收「顺序读得回来」）：`A→B` 与 `B→A` 落库后 `ord` 序列不同。
/// 变异形（两发都实测过）：① 让 `compute_chain_id` 先把环节**排序**再进 preimage（= 「不区分方向」）
/// ⇒ 两条链算出同一个 id，第二条被吞 ⇒ 本条红在返回条数那句（读数 1）；② 写侧把环节的 `ord`
/// 写成常量 0 ⇒ 本条红在「两个不同的 ord 序列」那句（两头的序列塌成同一个），同发让 P3 与 P8
/// 红在各自的序号断言上。
#[test]
fn 反向的两条链落库后环节序列相反() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let (a, b) = (ids[0].clone(), ids[1].clone());
    let mech = "同一句机制，方向相反就是两条链";
    let rows = vec![chain(mech, vec![a.clone(), b.clone()], vec![]),
                    chain(mech, vec![b.clone(), a.clone()], vec![])];
    assert_eq!(put_chains(m, &rows, &ctx(), NOW).unwrap(), 2, "方向是链的身份的一部分");
    let heads = causal_heads(m);
    assert_eq!(heads.len(), 2);
    let mut seqs: Vec<Vec<String>> = heads.iter()
        .map(|h| link_seq(m, h).into_iter().map(|(_, uid)| uid).collect()).collect();
    // 两个链头的 hex id 谁先谁后与本条无关 ⇒ 先按环节序列本身定序再比。
    seqs.sort();
    assert_eq!(seqs, vec![vec![a.clone(), b.clone()], vec![b.clone(), a.clone()]],
        "A→B 与 B→A 必须在库里读得出两个不同的 ord 序列");
    for head in &heads {
        let ords: Vec<i64> = link_seq(m, head).iter().map(|(o, _)| *o).collect();
        assert_eq!(ords, vec![0, 1], "{head} 的环节序号必须从 0 起连续，常量 ord 就是把方向抹平");
    }
}

/// P3（工单 C-3）：`basis ⊆ link` 的链 ⇒ link 行在场、`ord` 完整，basis 那一跳不重复成行。
/// 变异形（本条实测过）：**交换 `put_chains` 里「先环节后依据」那两段循环** ⇒ 同一句柄先以
/// `role='basis'`（`ord` NULL）落库，后到的环节行撞 `(mem_table, mem_id, turn_uid)` 那个主键
/// 被 `INSERT OR IGNORE` 忽略 ⇒ 本条在「甲那一行仍是 link」与「ord 序列是 0..n 连续」两处一起红。
#[test]
fn 既是环节又是依据的那一句柄留在环节侧() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let (a, b, c) = (ids[0].clone(), ids[1].clone(), ids[2].clone());
    let rows = vec![chain("甲先于乙，丙只是它凭什么", vec![a.clone(), b.clone()],
                          vec![a.clone(), c.clone()])];
    assert_eq!(put_chains(m, &rows, &ctx(), NOW).unwrap(), 1);
    let head = causal_heads(m)[0].clone();
    let seq = link_seq(m, &head);
    assert_eq!(seq, vec![(0, a.clone()), (1, b.clone())],
        "环节必须按答复顺序带着 0..n 的 ord；甲若被依据行顶掉，这里就少一格");
    let all = prov_rows(m, &head);
    assert_eq!(all.len(), 3, "两格环节 + 一格纯依据 = 三行；既是环节又是依据的那一格不许重复成行");
    let row = all.iter().find(|r| r.0 == a).unwrap().clone();
    assert_eq!((row.1.as_str(), row.3.as_str()), ("link", "card"),
        "甲既是环节又是依据 ⇒ 库里留下的必须是环节那一行（C-3），ref_kind 是被引行所在的表");
    assert_eq!(row.2, Some(0), "环节带序是那条跨列 CHECK 的另一半");
    let basis: Vec<String> = all.iter().filter(|r| r.1 == "basis").map(|r| r.0.clone()).collect();
    assert_eq!(basis, vec![c.clone()], "只有不在环节里的那一格才以依据的身份落库");
}

/// P4（I-3 同律）：只读句柄 ⇒ `MemError::ReadOnly`，不是 sqlite 那句文本。
/// 变异形（本条实测过）：删掉 `put_chains` 开头那一句只读闸门 ⇒ 引擎仍然拒写，但错出来的是
/// `MemError::Sql("attempt to write a readonly database")`，本条按变体名红（T5 分诊读的是族名）。
#[test]
fn 只读句柄念出的是_readonly_而不是_sqlite_的文本() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    let ids = {
        let root = Root::open(dir.path()).unwrap();
        seed3(root.memory())
    };
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = rc_store::AgentRoot::open_replica(dir.path(), &snap).unwrap();
    assert!(ar.memory().is_read_only(), "夹具前提破了：副本句柄不是只读的，本条就没在测闸门");
    let rows = vec![chain("副本上写不进任何东西", vec![ids[0].clone(), ids[1].clone()], vec![])];
    let e = put_chains(ar.memory(), &rows, &ctx(), NOW).unwrap_err();
    assert!(matches!(e, MemError::ReadOnly), "只读句柄要念族名，拿到 {e:?}");
    assert_eq!(scalar_i64(ar.memory(), "SELECT count(*) FROM card WHERE kind='causal'"), 0, "拒写之后不许留下半行");
}

/// P5（派发档 §1b③ 写侧那道「守在库门口」）：绕过 parse 直喂空 mechanism ⇒ `ContentEmpty` 且库里零行。
/// 用例形只许 `""` 与 `"   "`：零宽字符那一形 `trim()` 与 `normalize_body` 两道都放行（工单 §2
/// 已取证），写成用例就是给一条会说谎的正文记一条「被拦住了」。
/// 变异形（本条实测过）：删掉那句 `normalize_body(&c.mechanism).is_empty()`（连带它外面那个
/// 预检循环）⇒ 本条在 `unwrap_err()` 处红（空正文的因果事件真的落进库）。
/// 混合批那一句钉的是"坏一条 = 整批不写"这一承诺的**可观察形状**（库里零行）。它不声称能分辨
/// 判据的位置：把预检挪进逐条循环之后事务回滚仍然兜得住，两种写法的库面读数相同 ⇒ 这一格的
/// 差别只在代码里，不在读数里，本条不替它编牙。
#[test]
fn 绕过_parse_直喂的空机制在库门口就被拦住() {
    for mechanism in ["", "   "] {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let ids = seed3(m);
        let e = put_chains(m, &[chain(mechanism, vec![ids[0].clone(), ids[1].clone()], vec![])],
                           &ctx(), NOW).unwrap_err();
        assert!(matches!(e, MemError::ContentEmpty),
                "空机制（形如 {mechanism:?}）要念 ContentEmpty，拿到 {e:?}");
        assert_eq!(scalar_i64(m, "SELECT count(*) FROM card WHERE kind='causal'"), 0, "空机制那条不许留下头行");
        assert_eq!(scalar_i64(m, "SELECT count(*) FROM provenance WHERE ref_kind != 'turn'"), 0,
            "头行没落，环节/依据行也不许落");
    }
    // 混合批：判据在任何一笔写入之前 ⇒ 合法的那一条也整批不落（`put_batch` 的「先查后写」同律）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let mixed = vec![chain("合法的那一条", vec![ids[0].clone(), ids[1].clone()], vec![]),
                     chain("  ", vec![ids[1].clone(), ids[2].clone()], vec![])];
    assert!(matches!(put_chains(m, &mixed, &ctx(), NOW).unwrap_err(), MemError::ContentEmpty));
    assert_eq!(scalar_i64(m, "SELECT count(*) FROM card WHERE kind='causal'"), 0, "坏一条 = 整批不写，不留半批");
    assert_eq!(scalar_i64(m, "SELECT count(*) FROM provenance WHERE ref_kind != 'turn'"), 0);
}

/// P6（幂等可观测）：同一条链重跑 ⇒ `put_chains` 返回 0。
/// 变异形（本条实测过）：把那句 `if n_ins == 0 { continue; }` 改成「照旧累加条数」（= 返回计划条数）
/// ⇒ 第二次调用报 1，本条红；库里行数不变，所以只有这个读数能看见「重复批看起来像又落了一批」。
#[test]
fn 同一条链重跑报零() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let rows = vec![chain("先立的决定顶掉排期", vec![ids[0].clone(), ids[1].clone()],
                          vec![ids[2].clone()])];
    assert_eq!(put_chains(m, &rows, &ctx(), NOW).unwrap(), 1, "第一遍确实落了一条链");
    let heads = causal_heads(m);
    assert_eq!(heads.len(), 1);
    assert_eq!(put_chains(m, &rows, &ctx(), NOW).unwrap(), 0,
        "重跑必须报 0：报 1 就是「计划条数」，幂等这一格从此不可观测");
    assert_eq!(put_chains(m, &rows, &ctx(), NOW + 1).unwrap(), 0,
        "换了写入钟也不许数出第二条链（头行 id 不含 created_at）");
    let head = &heads[0];
    assert_eq!(link_seq(m, head).len(), 2, "重跑不许把环节行插成双份");
    assert_eq!(prov_rows(m, head).len(), 3, "头行的出处行总数在重跑后仍是三行");
}

/// P7（裁定 13 + I5）：链头行的时间锚取**最后一环的源侧时刻**（`created_at_ms`），
/// 结论是 `anchored`；它既不进审核游标也不进推导池。
/// I5 之前这里断言的是 `NULL/NULL/'unknown'`；补 turn 时间锚之后，链头的时刻来自真实环节，
/// 不再是导入钟也不是猜出来的"现在"。
/// 变异形（本条实测过）：把 `insert_derived_provenance` 的 `ref_kind` 实参递成 `'turn'` ⇒ 链头行
/// 被读成「抽取写的行」，本条在 `unreviewed_rows` 那句红（`rows_since` 用的是同一支谓词，两句
/// 都在本条里；红只报第一个未通过的断言）。同一发也让 P3 的 `ref_kind` 那句红——那里钉的是
/// "表前缀"这一半。
#[test]
fn 链头行锚在最后一环的源时刻且不进游标也不进推导池() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let rows = vec![chain("甲把乙顶掉了", vec![ids[0].clone(), ids[1].clone()], vec![ids[2].clone()])];
    assert_eq!(put_chains(m, &rows, &ctx(), NOW).unwrap(), 1);
    let head = causal_heads(m)[0].clone();
    let (ms, anchor_id, conf, verdict, kind, title):
        (Option<i64>, Option<String>, String, String, String, String) = m.conn().query_row(
        "SELECT time_anchor_ms, time_anchor_id, time_confidence, verdict, kind, title
         FROM card WHERE id=?1",
        [&head], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap();
    assert_eq!(ms, Some(2000), "I5：锚 = 最后一环（card b，created_at=2000）的源侧时刻");
    assert_eq!(anchor_id.as_deref(), Some(ids[1].as_str()), "锚行 = 链的最后一环");
    assert_eq!(conf, "anchored", "有时刻锚 ⇒ anchored");
    assert_eq!((kind.as_str(), verdict.as_str()), ("causal", "unreviewed"),
        "夹具前提：头行的 verdict 确实是未审的默认值 ⇒ 看不见它才是谓词在筛，不是行没落");
    assert_eq!(title, "derive-甲把乙顶掉了", "I5 的标题形状 = derive- 前缀 + 完整机制句（不截断）");
    let cursor: Vec<String> = reader::unreviewed_rows(m, 100).unwrap()
        .into_iter().map(|r| r.key.id).collect();
    assert!(!cursor.contains(&head), "链头是别的 pass 的结论，不该再花一次判定额度");
    let pool: Vec<String> = reader::rows_since(m, None, 100).unwrap()
        .into_iter().map(|r| r.key.id).collect();
    assert!(!pool.contains(&head), "链头不回喂下一轮推导批（零回灌那一格）");
    assert_eq!(cursor.len(), 3, "三张种子 card 照旧在游标里：看不见链头不是「整个游标空了」");
    assert_eq!(pool.len(), 3, "推导池同样只剩抽取行");
}

/// P8（值域由写侧把守）：`SELECT DISTINCT role FROM provenance` 只读出 `PROVENANCE_ROLES` 里
/// 那两个值，且每个头行的 `link` 序列是 `0..n` 连续。
/// 变异形（本条实测过）：把 `ROLE_LINK` 拼成 `"inks"` ⇒ 环节行同时违那条跨列 CHECK，而插入用的
/// 是 `INSERT OR IGNORE`（SQLite 的冲突消解同样管 CHECK），于是**整行被静默跳过、`put_chains`
/// 照返 Ok**（实测读数：`DISTINCT role` 只剩 `["basis"]`、每个头行的环节序列是空集，P1/P2/P3/P6
/// 各自的环节断言一起红）；把 `ROLE_BASIS` 拼成一个域外词 ⇒ CHECK 放行（它只管 role↔ord 的
/// 耦合、不管值域），库里读回第三个词，本条在 `distinct` 那句红、P3 的依据那句同红。
/// 两发都不报错 ⇒ 值域这一半确实只有这里的读数在守。
#[test]
fn role_列只装得下那两个_slug_且环节序号连续() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let rows = vec![chain("第一条链的机制句", vec![ids[0].clone(), ids[1].clone()], vec![ids[2].clone()]),
                    chain("第二条链的机制句", vec![ids[2].clone(), ids[0].clone(), ids[1].clone()], vec![])];
    assert_eq!(put_chains(m, &rows, &ctx(), NOW).unwrap(), 2);
    let mut st = m.conn().prepare("SELECT DISTINCT role FROM provenance ORDER BY role").unwrap();
    let distinct: Vec<String> = st.query_map([], |r| r.get::<_, String>(0))
        .unwrap().map(|x| x.unwrap()).collect();
    let mut want = PROVENANCE_ROLES.to_vec();
    want.sort();
    assert_eq!(distinct, want,
        "provenance.role 的值域只能由写侧的常量把守：库里存得下别的词，这一格不红就等于没守");
    for head in causal_heads(m) {
        let seq = link_seq(m, &head);
        let ords: Vec<i64> = seq.iter().map(|(o, _)| *o).collect();
        assert_eq!(ords, (0..seq.len() as i64).collect::<Vec<i64>>(),
            "{head} 的环节序号必须 0..n 连续（缺 0 就是环节被依据行顶掉的那一种）");
        assert!(!seq.is_empty(), "{head} 一个环节都没有 ⇒ 那条链落库就是一行说不出方向的头行");
    }
    assert_eq!(scalar_i64(m, "SELECT count(*) FROM provenance WHERE role='link'"), 5,
        "两条链的环节行数之和（2 + 3）：basis 那一跳与环节同句柄时不重复成行（C-3）");
}

/// 批次 M 段 B：`ChainContent.causal_type` 必须落到链头行的 `event.causal_type` 列上。
/// 两条链分别 `causes` / `prevents` ⇒ 按头行 id 读回的类型逐字对（读的是**那一行**，
/// 不是"库里出现过某个值"——两条同值的夹具分不出这一格）。
/// 再用一条 `UPDATE ... SET causal_type='bogus'` 证明库里的 CHECK 仍在执法：
/// 写侧没有绕过那一列（若写侧 SQL 拼错列名，这里的 UPDATE 会成功而红）。
/// 变异形（M-B6 实测过）：`put_chains` 的参数把 `c.causal_type` 换成 `None::<String>`
/// ⇒ 两行都读出 NULL ⇒ 本条前半红。
#[test]
fn chain_type_lands_on_the_head_row() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let mut causes = chain("甲引出乙", vec![ids[0].clone(), ids[1].clone()], vec![]);
    causes.causal_type = "causes".into();
    let mut prevents = chain("丙挡住乙", vec![ids[2].clone(), ids[1].clone()], vec![]);
    prevents.causal_type = "prevents".into();
    assert_eq!(put_chains(m, &[causes, prevents], &ctx(), NOW).unwrap(), 2);
    for (mechanism, want) in [("甲引出乙", "causes"), ("丙挡住乙", "prevents")] {
        let got: Option<String> = m.conn().query_row(
            "SELECT causal_type FROM card WHERE kind='causal' AND text=?1",
            [mechanism], |r| r.get(0)).unwrap();
        assert_eq!(got.as_deref(), Some(want),
            "链头 {mechanism:?} 的类型必须逐字落库：causal_type 在写侧被绕过或写错列");
    }
    // 库里的 CHECK 是最后一道：绕过写入口直接改一个域外值必须被拒。
    let e = m.conn().execute("UPDATE card SET causal_type='bogus' WHERE kind='causal'", []).unwrap_err();
    assert!(e.to_string().contains("CHECK"), "causal_type 的 CHECK 没在执法：{e}");
}

/// I5：标题 = `derive-` + **完整**机制句（不截断），时间锚 = 最后一环的源侧时刻。
#[test]
fn 长机制句的标题完整且时间锚取最后一环() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let mech = "压缩阈值从零点五提高到零点七五之后压缩就会在约三百八十四K处触发并保留上下文";
    assert!(mech.chars().count() > 20, "夹具前提：机制句必须超过旧的 20 字符截断线");
    assert_eq!(put_chains(m, &[chain(mech, vec![ids[0].clone(), ids[1].clone()], vec![])], &ctx(), NOW)
        .unwrap(), 1);
    let (title, as_of, created, anchor_ms, anchor_id, conf): (String, i64, i64, Option<i64>, Option<String>, String) =
        m.conn().query_row(
            "SELECT title, as_of_ms, created_at_ms, time_anchor_ms, time_anchor_id, time_confidence
             FROM card WHERE kind='causal'", [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap();
    assert_eq!(title, format!("derive-{mech}"), "I5：标题不截断");
    assert_eq!(anchor_ms, Some(2000), "锚 = 最后一环 b 的源侧时刻");
    assert_eq!(anchor_id.as_deref(), Some(ids[1].as_str()));
    assert_eq!((as_of, created, conf.as_str()), (2000, 2000, "anchored"),
        "as_of/created_at 取锚行时刻，不取导入钟");
}

/// I5：同一 `causal_type` + 同一顺序的环节序列 ⇒ 只留一条链；换个说法的重复必须合并。
/// C-1（同机制不同环节）与 P2（A→B / B→A）不受影响：环节序列不同。
#[test]
fn 同环节序列的重复因果事件被合并() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    let first = chain("先立的决定顶掉排期", vec![ids[0].clone(), ids[1].clone()], vec![]);
    assert_eq!(put_chains_merged(m, &[first], &ctx(), NOW).unwrap(), (1, 0));
    // 同一组环节、同一方向、换一句机制 ⇒ 重复（不是新因果）。
    let reworded = chain("排期被更早的那条决定顶掉了", vec![ids[0].clone(), ids[1].clone()], vec![]);
    assert_eq!(put_chains_merged(m, &[reworded], &ctx(), NOW).unwrap(), (0, 1),
        "同环节序列 ⇒ 合并计数 1，不落第二行");
    let n: i64 = m.conn().query_row("SELECT count(*) FROM card WHERE kind='causal'", [],
                                    |r| r.get(0)).unwrap();
    assert_eq!(n, 1, "库里只留一条因果头行");
}

/// I5 的文本降权：机制句就是某条环节 card 的正文 ⇒ 复述，不是新因果。
#[test]
fn 机制句复述环节正文时不落头行() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let ids = seed3(m);
    // seed3 的 a 行正文 = "甲这一行的正文"
    let c = chain("甲这一行的正文", vec![ids[0].clone(), ids[1].clone()], vec![]);
    assert_eq!(put_chains_merged(m, &[c], &ctx(), NOW).unwrap(), (0, 1),
        "机制与环节正文逐字相同 ⇒ 降权合并");
    let n: i64 = m.conn().query_row("SELECT count(*) FROM card WHERE kind='causal'", [],
                                    |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "复述不落行；它没有贡献新的因果信息");
}
