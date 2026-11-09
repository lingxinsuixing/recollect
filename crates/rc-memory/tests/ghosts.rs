//! A40 的重影面：Q1 单边指认 → 闭包互挂 → 收敛；Q2 歧义只报不动；Q3 同题多行点名。
//! 编号逐字对齐 outline §6.7 的探针表（t1–t5），外加两条本计划补的边界（自指别名、只读副本）。
//! 断言全部是具体数字与具体 id：`linked`、`raised`、`resolved`、别名表的行数与内容。

use rc_memory::anomaly::{sweep_ghosts, GhostReport, SweepReport};
use rc_memory::keys::{AnchorRole, MemKey, Scope, ScopeKind};
use rc_memory::reader::{ambiguous_alias_claimants, duplicate_title_rows, entity_directory,
                        unclosed_alias_pairs};
use rc_memory::redirect::merge_redirect;
use rc_memory::writers_x::{link_alias_pair, put_card, put_entity, EntityContent, FactContent};
use rc_memory::{retire, MemError, Provenance};
use rc_store::{AgentRoot, MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;

fn open() -> (tempfile::TempDir, Root) {
    let d = tempfile::tempdir().unwrap();
    let root = Root::open(d.path()).unwrap();
    (d, root)
}
fn prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "t-ghost".repeat(8), anchor_role: AnchorRole::Human }]
}
/// 合成实体行。归属固定 `Person/p-ghost`：Q3 的组按 (表, 归属, 标题) 聚，把归属钉成常数
/// 才能让"同题"这一个轴自己说话。`kind` 是形参——A39 的病就是 kind 不归一直接分裂 id。
fn ent(m: &MemoryHandle, title: &str, aliases: &[&str], kind: &str, at: i64) -> MemKey {
    put_entity(m, &EntityContent {
        kind: kind.into(), title: title.into(),
        scope: Scope { kind: ScopeKind::Person, key: "p-ghost".into() },
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
    }, &prov(), at).unwrap()
}
/// 跨表同名的那一只。归属与 `ent` **同一个常数**（`Person/p-ghost`）：Q3 的组键是 (表, 归属, 标题)，
/// 卡若落在另一个归属里，"撤掉同表那一支"的变异（Step 7 的 ⑤）就打不到它，
/// "跨表同名不算重影"这条口径因此只剩注释在守（它必须是判据）。
fn card(m: &MemoryHandle, title: &str, body: &str) -> MemKey {
    put_card(m, &FactContent {
        kind: "note".into(), title: title.into(), body: body.into(),
        scope: Scope { kind: ScopeKind::Person, key: "p-ghost".into() }, valid_to_ms: None,
    }, &prov(), NOW).unwrap()
}
/// 别名表全量，渲染成 `entity_id|alias` 后排序——`id` 是内容寻址的 hex，SQL 的
/// `ORDER BY entity_id, alias` 给不出**我可预**的序，所以两边都排一次再比。
/// 排序不弱化判据：要断的是"哪几对存在"这个集合，而集合的基数另有一行 `alias_count`。
fn alias_pairs(m: &MemoryHandle) -> Vec<String> {
    let mut st = m.conn().prepare("SELECT anchor_id, alias FROM alias").unwrap();
    let mut v = st.query_map([], |r| {
        let id: String = r.get(0)?;
        let a: String = r.get(1)?;
        Ok(format!("{id}|{a}"))
    }).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    v.sort();
    v
}
fn alias_count(m: &MemoryHandle) -> i64 {
    m.conn().query_row("SELECT count(*) FROM alias", [], |r| r.get(0)).unwrap()
}
/// 某一族当前挂着未解决的告警目标 id（升/解两条路径都读它）。
fn open_ids(m: &MemoryHandle, kind_slug: &str) -> Vec<String> {
    let mut st = m.conn().prepare(
        "SELECT mem_id FROM attention WHERE kind=?1 AND resolved_at_ms IS NULL ORDER BY mem_id").unwrap();
    st.query_map([kind_slug], |r| r.get::<_, String>(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap()
}
fn sorted(v: Vec<String>) -> Vec<String> { let mut v = v; v.sort(); v }

#[test]
fn t1_单边指认被闭包_闭包后不复燃_重放幂等() {
    let (_d, root) = open(); let m = root.memory();
    let a = ent(m, "老王", &["王工"], "person", NOW);   // 认领方
    let b = ent(m, "王工", &[], "person", NOW);         // 自成一行
    assert_eq!(unclosed_alias_pairs(m).unwrap(), vec![(a.clone(), b.clone())],
        "单边态必须检出这一对（e=认领方, g=自成行方）");
    let rep = sweep_ghosts(m, NOW + 1).unwrap();
    assert_eq!(rep.linked, 1, "闭包只补反向那一条：正向那条本来就在表里");
    assert_eq!(rep.ambiguity, SweepReport { raised: 0, resolved: 0 }, "一家认领不算歧义");
    assert_eq!(unclosed_alias_pairs(m).unwrap().len(), 0,
        "闭包后必须归零——这一条是『无反向 alias』那支收敛项的唯一牙");
    assert_eq!(alias_pairs(m), sorted(vec![format!("{}|王工", a.id), format!("{}|老王", b.id)]),
        "闭包的结果是各持对侧标题（不是两边都挂全两个名字）");
    // A40 的缺口是检索面的：查"老王"原本不中 B。这里按 `rc-agent::tools` 的
    // name-OR-alias 规则**复述**一遍命中集（不是新读口，真消费面的用例在 rc-agent 那边）。
    let dir = entity_directory(m).unwrap();
    let hits = |q: &str| dir.iter()
        .filter(|e| e.name == q || e.aliases.iter().any(|x| x == q)).count();
    assert_eq!((hits("老王"), hits("王工")), (2, 2), "两侧互查都命中 = 闭包的客户价值");
    let second = sweep_ghosts(m, NOW + 2).unwrap();
    assert_eq!((second.linked, alias_count(m)), (0, 2),
        "重放幂等：一行都不许多（outline §1.1-T13 验收行点名的两条数值断言之一）");
}

#[test]
fn t2_反方向闭包_半闭包态重放安全() {
    let (_d, root) = open(); let m = root.memory();
    let a = ent(m, "老王", &[], "person", NOW);
    let b = ent(m, "王工", &["老王"], "person", NOW);   // 方向反过来
    assert_eq!(unclosed_alias_pairs(m).unwrap(), vec![(b.clone(), a.clone())],
        "反向同样检出：判据对称，不存在『只有认领方说了算』");
    let rep = sweep_ghosts(m, NOW + 1).unwrap();
    assert_eq!(rep.linked, 1, "半闭包态补的仍是缺的那一条");
    assert_eq!(unclosed_alias_pairs(m).unwrap().len(), 0);
    assert_eq!(alias_pairs(m), sorted(vec![format!("{}|老王", b.id), format!("{}|王工", a.id)]));
}

#[test]
fn t3_歧义只报不动_两家各挂一行告警() {
    let (_d, root) = open(); let m = root.memory();
    let a = ent(m, "老王", &["王工"], "person", NOW);
    let c = ent(m, "老张", &["王工"], "person", NOW);   // 第二家认领同一串
    ent(m, "王工", &[], "person", NOW);
    assert_eq!(unclosed_alias_pairs(m).unwrap().len(), 0,
        "有第二 claimant ⇒ 一家都不许闭包（A40『绝不猜身份』的动作面）");
    let hits = ambiguous_alias_claimants(m).unwrap();
    assert_eq!(hits.len(), 2, "两家各一格");
    assert!(hits.iter().all(|(_, s)| s == "王工"), "报的都是那一串");
    let rep = sweep_ghosts(m, NOW + 1).unwrap();
    assert_eq!((rep.linked, rep.ambiguity.raised, rep.ambiguity.resolved), (0, 2, 0));
    assert_eq!(alias_count(m), 2, "只报不动：别名表一行都不许多");
    assert_eq!(open_ids(m, "alias_ambiguity"), sorted(vec![a.id.clone(), c.id.clone()]),
        "告警点在两家身上（一家都不漏、一家都不多）");
    let second = sweep_ghosts(m, NOW + 2).unwrap();
    assert_eq!((second.linked, second.ambiguity), (0, SweepReport { raised: 0, resolved: 0 }),
        "重复扫不升不降：写前查重与解除面同时成立");
}

#[test]
fn t4_零噪音_普通别名_自指别名_跨表同名都不报() {
    let (_d, root) = open(); let m = root.memory();
    ent(m, "老王", &["王小姐"], "person", NOW);         // 普通别名，指不到真行
    assert_eq!((unclosed_alias_pairs(m).unwrap().len(),
                ambiguous_alias_claimants(m).unwrap().len()), (0, 0),
        "普通别名零命中：Q1 要有对侧行、Q2 要有第二家，两条都不成立");
    ent(m, "阿明", &["阿明"], "person", NOW);           // 别名撞自己的标题 = 本人
    assert_eq!(unclosed_alias_pairs(m).unwrap().len(), 0, "自指别名不许配成一对");
    ent(m, "老王", &[], "agent", NOW + 1);             // 与上面同名不同 kind
    card(m, "老王", "合成正文，不含任何生产文本");        // 跨表同名
    let got = duplicate_title_rows(m).unwrap();
    assert_eq!(got.len(), 2, "同表同归属同标题的两行才报（实体两行），跨表的卡不卷进来");
    assert!(got.iter().all(|k| k.key_string().starts_with("anchor:")),
        "报出的两格都必须是锚行，实际 {got:?}");
}

#[test]
fn t5_同题多行两格都点名_升成_duplicate_title() {
    let (_d, root) = open(); let m = root.memory();
    let a = ent(m, "陈姐", &[], "person", NOW);
    let b = ent(m, "陈姐", &[], "agent", NOW + 1);   // A39 的病：kind 不归一 ⇒ 两个 id 两行
    let got: Vec<String> = duplicate_title_rows(m).unwrap().into_iter().map(|k| k.id).collect();
    assert_eq!(got, sorted(vec![a.id.clone(), b.id.clone()]),
        "几行就点名几行——只报一条会让 agent 以为重影只有一个方向");
    let rep = sweep_ghosts(m, NOW + 2).unwrap();
    assert_eq!((rep.linked, rep.duplicate_title.raised, rep.duplicate_title.resolved), (0, 2, 0));
    assert_eq!(open_ids(m, "duplicate_title"), sorted(vec![a.id.clone(), b.id.clone()]));
}

#[test]
fn t6_机械解除_两族各解各的账() {
    // 设计 §3.2 的 attention 那条要求每个 kind 有配对的解除路径。两族各自验一遍，且**互不解除对方的行**。
    let (_d, root) = open(); let m = root.memory();
    let a = ent(m, "老王", &["王工"], "person", NOW);
    let c = ent(m, "老张", &["王工"], "person", NOW);
    ent(m, "王工", &[], "person", NOW);
    sweep_ghosts(m, NOW + 1).unwrap();
    assert_eq!(open_ids(m, "alias_ambiguity").len(), 2, "先让歧义挂着");
    let d = ent(m, "陈姐", &[], "person", NOW + 1);
    let e = ent(m, "陈姐", &[], "agent", NOW + 1);
    sweep_ghosts(m, NOW + 2).unwrap();
    assert_eq!(open_ids(m, "duplicate_title").len(), 2, "两族同时挂着，才好分谁解谁的");

    retire(m, &c, "manual", NOW + 3).unwrap();          // 退掉一家 claimant
    let rep = sweep_ghosts(m, NOW + 4).unwrap();
    assert_eq!(rep.ambiguity, SweepReport { raised: 0, resolved: 2 },
        "两家一起解：幸存那家不再有成对的重影，被退那家的账由退场自己说");
    assert_eq!(open_ids(m, "alias_ambiguity"), Vec::<String>::new());
    assert_eq!(rep.duplicate_title, SweepReport { raised: 0, resolved: 0 },
        "歧义那一支不许碰同题族的账（形状保证，见 `sweep_family` 的 kind 定位）");

    retire(m, &e, "manual", NOW + 5).unwrap();          // 再退掉同题的一行
    let rep2 = sweep_ghosts(m, NOW + 6).unwrap();
    assert_eq!(rep2.duplicate_title, SweepReport { raised: 0, resolved: 2 });
    assert_eq!(open_ids(m, "duplicate_title"), Vec::<String>::new());
    let _ = (a, d);                                     // 两个幸存句柄只为断言 id 用得到
}

#[test]
fn 空库_一轮重影作业全零且不报错() {
    let (_d, root) = open(); let m = root.memory();
    let rep = sweep_ghosts(m, NOW).unwrap();
    assert_eq!(rep, GhostReport {
        linked: 0,
        ambiguity: SweepReport { raised: 0, resolved: 0 },
        duplicate_title: SweepReport { raised: 0, resolved: 0 },
    });
    assert_eq!((unclosed_alias_pairs(m).unwrap().len(),
                ambiguous_alias_claimants(m).unwrap().len(),
                duplicate_title_rows(m).unwrap().len()), (0, 0, 0));
}

/// 只读副本：`sweep_ghosts` 的第一个分支就是 `ReadOnly`（它的第一步是写），
/// 三条读口在副本上照给同一个数。
/// **`Root` 必须先进块作用域关掉**再 `fs::copy`——记忆库是 WAL，句柄没关就拷，
/// 拷出去的是没有主库内容的空壳，于是"拒绝写"这条断言会绿在错误的地方（T11 同形教训）。
#[test]
fn 只读副本上作业拒_三条读口照给同一个数() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    let (pairs, claims, dups, a, b);
    {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        a = ent(m, "老王", &["王工"], "person", NOW);
        b = ent(m, "王工", &[], "person", NOW);
        ent(m, "陈姐", &[], "person", NOW);
        ent(m, "陈姐", &[], "agent", NOW);
        pairs = unclosed_alias_pairs(m).unwrap().len();
        claims = ambiguous_alias_claimants(m).unwrap().len();
        dups = duplicate_title_rows(m).unwrap().len();
    }
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    assert_eq!((pairs, claims, dups), (1, 0, 2), "副本这一侧的夹具形状先自证");
    let ar = AgentRoot::open_replica(dir.path(), &snap).unwrap();
    let m2 = ar.memory();
    assert!(matches!(sweep_ghosts(m2, NOW + 1), Err(MemError::ReadOnly)),
            "只读面必须拒绝作业，实际 {:?}", sweep_ghosts(m2, NOW + 1).err());
    assert!(matches!(link_alias_pair(m2, &a, &b), Err(MemError::ReadOnly)));
    assert_eq!((unclosed_alias_pairs(m2).unwrap().len(),
                ambiguous_alias_claimants(m2).unwrap().len(),
                duplicate_title_rows(m2).unwrap().len()), (pairs, claims, dups),
        "读口在副本上与 rw 侧同读数（否则『agent 拿副本自查』这条使用面就断了）");
}

/// 顺序的证人：闭包写把**对侧标题**挂成本方的别名，所以它**能**给同一串别名新增第二个认领方，
/// 而那一格必须先写完才读得见——`sweep_ghosts` 的"闭包在前"因此不是口径偏好，是被这条断言钉住的。
/// 夹具是两条单边指认串成一条链：`A(老王)` 认领 "王工" = `B` 的标题，`D(老张)` 认领 "老王" = `A` 的标题。
/// 两条闭包分别写入 `(B, "老王")` 与 `(A, "老张")` ⇒ "老王" 的活 claimant 变成 `{B, D}` = 2 ⇒ Q2 本轮升两行。
/// 变异（判据的牙）：把 `sweep_ghosts` 里 Q2 的扫描与 `sweep_family` 挪到闭包循环**之前**
/// ⇒ `ambiguity.raised` 拿到 **0**（这一轮库已经在吵，报告却说干净）。
#[test]
fn t9_闭包给同一串新增认领方_所以必须先写后读() {
    let (_d, root) = open();
    let m = root.memory();
    let a = ent(m, "老王", &["王工"], "person", NOW);
    let b = ent(m, "王工", &[], "person", NOW);
    let d = ent(m, "老张", &["老王"], "person", NOW);
    let pairs = unclosed_alias_pairs(m).unwrap();
    assert_eq!(pairs.len(), 2, "两条单边指认都要检出（A,B) 与 (D,A)");
    assert!(pairs.contains(&(a.clone(), b.clone())) && pairs.contains(&(d.clone(), a.clone())),
        "检出的必须是这两对，实际 {pairs:?}");
    assert_eq!(ambiguous_alias_claimants(m).unwrap().len(), 0,
        "闭包前没有歧义——这一格就是顺序之所以有关的全部理由");
    let rep = sweep_ghosts(m, NOW + 1).unwrap();
    assert_eq!(rep.linked, 2, "每条闭包各补一条反向（正向那条被 INSERT OR IGNORE 折掉）");
    assert_eq!(rep.ambiguity.raised, 2, "闭包新增的认领方必须本轮就报出来");
    assert_eq!(sorted(open_ids(m, "alias_ambiguity")), sorted(vec![b.id.clone(), d.id.clone()]),
        "点名的是新认领方 B 与原认领方 D，不是被认领的那一行 A");
    assert_eq!(rep.duplicate_title, SweepReport { raised: 0, resolved: 0 }, "三个标题互不相同");
    assert_eq!(unclosed_alias_pairs(m).unwrap().len(), 0, "闭包仍在一轮内收敛");
    assert_eq!(sweep_ghosts(m, NOW + 2).unwrap().ambiguity.raised, 0, "重放不重复升账");
}

/// 3E 收口批 · "精确串等"这条边界的证人（登记9：既有夹具不含归一差异 ⇒ 边界只剩注释在守）。
/// `normalize_body` 的三件事里 **trim** 最容易在库里造出来（标题与别名都是原样 `INSERT`，
/// 归一只发生在 `compute_mem_id` 的 preimage 里），所以差一个尾随空格就是"归一之后同串"。
/// 两族各配一组正对照——正对照在场 ⇒ 报出的那一组不是"读口整个坏了"，本条断的是**只漏不误**。
/// 标题那两行必须靠 `kind` 分开：`compute_mem_id` 对标题先归一，同 kind 同题的两种写法算出
/// **同一个 id** ⇒ `ON CONFLICT DO NOTHING` 折叠成一行，夹具自己空过（`assert_ne!` 钉住它）。
/// 牙（两道，各打一处比较）：`unclosed_alias_pairs` 的 `g.title = al.alias` 与
/// `duplicate_title_rows` 的 `o.title = l.title` 各自换成 `TRIM(…) = TRIM(…)`
/// ⇒ 对应那一侧的读数凭空多出一格而红，另一侧不受影响。
#[test]
fn t10_标题仅差归一_精确串等只漏不误() {
    let (_d, root) = open(); let m = root.memory();
    // Q3：逐字同题的一对（正对照，必须报）+ 仅差尾随空格的一对（必须不报）。
    let p1 = ent(m, "陈姐", &[], "person", NOW);
    let p2 = ent(m, "陈姐", &[], "agent", NOW + 1);
    let q1 = ent(m, "赵姨", &[], "person", NOW + 1);
    let q2 = ent(m, "赵姨 ", &[], "agent", NOW + 1);
    assert_ne!(p1.id, p2.id, "正对照自己没造出两行 ⇒ 本条空过");
    assert_ne!(q1.id, q2.id, "归一差那对自己没造出两行 ⇒ 本条空过");
    // Q1：逐字对得上的单边指认（正对照，必须配对）+ 别名多一个尾随空格的那条（必须不配）。
    let c = ent(m, "王工", &[], "person", NOW + 1);
    let d = ent(m, "老张", &["王工"], "person", NOW + 1);
    let e = ent(m, "李姐", &[], "person", NOW + 1);
    let f = ent(m, "周哥", &["李姐 "], "person", NOW + 1);
    let _ = (e.clone(), f.clone());   // 归一差那一对只撑夹具形状，后面两条断言都不读它们的句柄

    let got = sorted(duplicate_title_rows(m).unwrap().into_iter().map(|k| k.id).collect());
    assert_eq!(got, sorted(vec![p1.id.clone(), p2.id.clone()]),
        "只报逐字同题的那两行；仅差归一的赵姨两行不许成组（成组了就是把口径改成了归一匹配）");
    assert_eq!(unclosed_alias_pairs(m).unwrap(), vec![(d.clone(), c.clone())],
        "只配逐字相等的那一对；别名差一个尾随空格就不算指认");
}

// ── W29：已合并的锚不再认领名字 ───────────────────────────────────────────────
//
// 病：`alive_where(Anchor)` 只筛退场/顶替/否决三本账，**不看 `status`**；redirect（方案二）
// 零搬运 ⇒ 旧锚行、旧锚别名行都留在原处，只把 `status` 盖成 `merged`。于是 merged 的旧锚
// 继续在 Q2/Q3 里"认领"名字：每合并一次就长出一族永久告警，扫一轮解不掉、下一轮又升。
// 这三条用例的夹具都只用产品写入口（`put_entity` / `merge_redirect`），不手写 UPDATE status。

/// 三格**同正名**、同归属、三种 `kind`（否则内容寻址把同 kind 同题折成一行，夹具自己空过）。
/// 别名故意做成"两格共享 `龙哥` + 两格共享 `示例主人`"`：合并前两族各报三格，合并后只该剩两格。
/// `b` 挂自己的正名当别名是**现网同形**（示例主人 上有 示例昵称 + 示例主人），不是凑数：
/// redirect 会把被并走那一格的正名并进幸存格的 alias，而这一格的正名与它相同 ⇒ 插入是 0 行，
/// 于是本条断的只剩"谁在认领"，不会被一次多出来的别名行搅浑。
fn 同题三格(m: &MemoryHandle) -> (MemKey, MemKey, MemKey) {
    let a = ent(m, "示例主人", &["龙哥"], "person", NOW);        // 将被并走的那一格
    let b = ent(m, "示例主人", &["龙哥", "示例主人"], "agent", NOW); // 幸存格
    let c = ent(m, "示例主人", &["示例主人"], "org", NOW);           // 另一格**活**锚，同名同别名
    (a, b, c)
}

/// Q2 是 `(anchor_id, alias)` 一格，一格锚挂两串共享别名就出两行；本文件的断言要的是
/// "哪些锚在认领"，所以 id 先收成集合（`open_ids` 读的是 attention，天然按锚去重）。
fn distinct(mut v: Vec<String>) -> Vec<String> { v.sort(); v.dedup(); v }

/// 阴性对照：**不合并**时两族照样报（三格全点名）。
/// 这一条钉的是"筛的是 `status='merged'`，不是把这两族整个关了"——把 `status NOT IN
/// ('merged','retired')` 换成恒真、或者把 anchor 支整段删掉，本条的对照就没意义了，
/// 而下面那条 t12 才是判据真正生效的地方。两族都断言到具体 id 集合。
#[test]
fn t11_阴性对照_未合并时同题家族照样三格全点名() {
    let (_d, root) = open(); let m = root.memory();
    let (a, b, c) = 同题三格(m);
    assert_eq!(unclosed_alias_pairs(m).unwrap().len(), 0,
        "夹具必须自己不动：两串别名要么撞不上标题、要么反方向已经互挂，闭包不许多写一行");
    let all = sorted(vec![a.id.clone(), b.id.clone(), c.id.clone()]);
    assert_eq!(sorted(duplicate_title_rows(m).unwrap().into_iter().map(|k| k.id).collect()), all,
        "同表同归属同正名的三格，一行都不许漏（这里还没有 merged，判据无从生效）");
    assert_eq!(distinct(ambiguous_alias_claimants(m).unwrap().into_iter().map(|(k, _)| k.id).collect()),
        all, "两串别名各由两家共享 ⇒ 三格都进 Q2");
    let rep = sweep_ghosts(m, NOW + 1).unwrap();
    assert_eq!((rep.ambiguity, rep.duplicate_title),
        (SweepReport { raised: 3, resolved: 0 }, SweepReport { raised: 3, resolved: 0 }),
        "未合并时两族各升三行：这一格是 t12 的阴性对照");
}

/// 判据 1：两格同正名的活锚，redirect 合并其中一格之后，`sweep_ghosts` **不因 merged 那格**
/// 升 `alias_ambiguity` / `duplicate_title`；幸存格与另一格同名活锚**照常报**。
///
/// 牙（两道）：① anchor 支的 `status NOT IN ('merged','retired')` 撤掉 ⇒ 两族各多一格 `a`，
/// 逐 id 的集合断言立刻红；② 把 anchor 支整段写成恒假（"把这一族关了"）⇒ 幸存格 `b` 与
/// 同名活锚 `c` 也一起消失，同样红。所以这一条同时钉住了"筛 merged"与"不是关族"。
#[test]
fn t12_合并后旧锚不再认领名字_幸存格与同名活锚仍照报() {
    let (_d, root) = open(); let m = root.memory();
    let (a, b, c) = 同题三格(m);
    let rep = merge_redirect(m, &a.id, &b.id, "w29-same-title", "agent", NOW + 1).unwrap();
    assert_eq!(rep.aliases_merged, 0, "两格正名相同 ⇒ redirect 的 alias 插入本来就该是 0 行");
    // 零搬运：旧锚自己的别名行还在（合并没有删它），所以下面两组归零只可能来自 status 筛。
    assert!(alias_pairs(m).contains(&format!("{}|龙哥", a.id)),
        "零搬运口径：merged 的旧锚别名行必须原样留在库里，实际 {:?}", alias_pairs(m));
    assert_eq!(unclosed_alias_pairs(m).unwrap().len(), 0, "合并后仍不许长出闭包对");

    let rep = sweep_ghosts(m, NOW + 2).unwrap();
    let survivors = sorted(vec![b.id.clone(), c.id.clone()]);
    assert_eq!(sorted(open_ids(m, "alias_ambiguity")), survivors,
        "只报活着的两家（`示例主人` 由 b 与 c 各持一份）：merged 的 a 一行都不许升，实际 {:?}",
        open_ids(m, "alias_ambiguity"));
    assert_eq!(sorted(open_ids(m, "duplicate_title")), survivors,
        "同题族同理：a 已经不是一行活锚，不许继续认领这个正名");
    assert_eq!((rep.ambiguity, rep.duplicate_title),
        (SweepReport { raised: 2, resolved: 0 }, SweepReport { raised: 2, resolved: 0 }),
        "两族各升两行（不是 3 行、也不是 0 行）");
    assert!(!open_ids(m, "alias_ambiguity").contains(&a.id), "merged 那一格不许出现在告警里");
    let again = sweep_ghosts(m, NOW + 3).unwrap();
    assert_eq!((again.ambiguity, again.duplicate_title),
        (SweepReport { raised: 0, resolved: 0 }, SweepReport { raised: 0, resolved: 0 }),
        "重放不升不解：告警不再随合并长出新的一族");
}

/// 判据 3：现网同形 `19d87129…`（merged，正名 示例昵称、别名 示例主人）+ `69ece1a1…`（active 示例主人，
/// 别名 示例主人）⇒ 合并之后 **0 条 alias 告警**。
///
/// 现网首扫的 2 条 `alias_ambiguity` 就是这两格：`示例主人` 由 merged 的旧锚与活锚各持一份。
/// 夹具按现网的形状复刻（旧锚 `示例主人` 别名、活锚自挂 `示例主人`、活锚正名 `示例主人`），三种读数都断言：
/// 合并前 2 格、合并收掉 2 格、合并后 0 格——同时钉住"旧锚的别名行还在"（零搬运）与
/// "redirect 把旧正名并进活锚 alias 恰好 +1"（现网那条 `alias+1`）。
#[test]
fn t13_现网同形_旧锚与活锚同名合并后零条alias告警() {
    let (_d, root) = open(); let m = root.memory();
    let old = ent(m, "示例昵称", &["示例主人"], "person", NOW);          // 19d87129…（将被 merged）
    let live = ent(m, "示例主人", &["示例主人"], "person", NOW);             // 69ece1a1…（活着的那一格）
    assert_eq!(unclosed_alias_pairs(m).unwrap().len(), 0,
        "现网同形里两侧已经互不相欠：旧锚认领 示例主人、活锚自持 示例主人，不生成闭包");
    let before = sweep_ghosts(m, NOW + 1).unwrap();
    assert_eq!(before.ambiguity, SweepReport { raised: 2, resolved: 0 },
        "合并前正是现网那 2 条：两家都认领 示例主人");
    assert_eq!(sorted(open_ids(m, "alias_ambiguity")), sorted(vec![old.id.clone(), live.id.clone()]));

    let merged = merge_redirect(m, &old.id, &live.id, "w29-live-shape", "agent", NOW + 2).unwrap();
    assert_eq!(merged.aliases_merged, 1, "旧正名 示例昵称 并进活锚，与现网的 alias+1 同形");
    assert!(alias_pairs(m).contains(&format!("{}|示例主人", old.id)),
        "零搬运：merged 旧锚自己那条 示例主人 别名仍在库里（否则归零是删除的功劳，不是判据的）");

    assert_eq!(ambiguous_alias_claimants(m).unwrap().len(), 0,
        "merged 的旧锚不再认领 示例主人 ⇒ 这一串只剩一家，Q2 一条都不该出");
    let after = sweep_ghosts(m, NOW + 3).unwrap();
    assert_eq!(after.ambiguity, SweepReport { raised: 0, resolved: 2 },
        "两行旧账由本轮机械解掉（判据不再成立 ⇒ 解，不需要谁去认证）");
    assert_eq!(open_ids(m, "alias_ambiguity"), Vec::<String>::new(), "这一族必须清空");
    assert_eq!(after.duplicate_title, SweepReport { raised: 0, resolved: 0 },
        "两格正名不同，同题族本来就该是 0（本条只压 alias 那一族）");
}
