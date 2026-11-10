//! §10 消费侧①的两半：副本 vs 活体、两次 vs 两次（任务 6 · Step 4）。
//!
//! **诚实标注**：跨 `arm64 Linux` 与 `x86_64 Windows` 那一半在本阶段只能做到"同一台机器、同一份字节"，
//! 因为环境里只有一个已安装 target；它押后到阶段 5，任何文档都不许写"已验证跨平台"。
//! 这里能断的是更强的那一半的**下限**：同一份字节经两个句柄（读写根 / 只读副本）出来，
//! 闸门决定必须逐行相同——不是"两边都不 panic"。

use rc_search::gate::{decide, probe_view, Form, GateCell};
use rc_search::rank::rank;
use rc_store::{AgentRoot, MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;
const Q: &str = "格子测试合成词";

fn shape(view: &rc_search::gate::ProbeView) -> (Vec<String>, Vec<(String, usize)>) {
    (
        view.forms.iter().map(|d| d.key.key_string()).collect(),
        view.silent.counts().into_iter().map(|(c, n)| (c.slug(), n)).collect(),
    )
}

/// 逐行的闸门决定：`(句柄, 形态, 格子)`。`ProbeView` 只有注入前缀与五格计数，
/// 它说不出"哪一行落在哪一格"——那正是副本对拍要问的东西，所以这里从 `rank` + `decide` 取。
fn decided_rows(m: &MemoryHandle) -> Vec<(String, Form, Option<GateCell>)> {
    rank(m, None, NOW, Q)
        .unwrap()
        .iter()
        .map(|s| {
            let d = decide(s, NOW);
            (d.key.key_string(), d.form, d.cell)
        })
        .collect()
}

/// 活库的一次读数：`(注入 id 串, 五格计数)` 的清单。起名字是为了躲 `clippy::type_complexity`
/// （它数的是"三层嵌套容器"这一形状，不是这份读数的含义）。
type LiveRead = Vec<(String, Vec<(String, usize)>)>;

/// 建一份"活库"：五格夹具 + 一批同分卡，跑一次索引。
/// 偏离 brief：`Root` 交回调用方并**让它活着**。原文里 `build_live` 一返回 `Root` 就被 drop，
/// 而 SQLite 在最后一个连接关闭时会自动 checkpoint 并把 `-wal` 收进主文件 ⇒
/// 复制前那句 `wal_checkpoint` 没有任何未落盘的页可管（Step 5 第 8 条在这份夹具上本来就无牙）。
/// 握着句柄才有真实形状：写入提交在 `-wal` 里、主文件还没收下它们。
fn build_live(dir: &std::path::Path) -> (Root, LiveRead) {
    let root = Root::open(dir).unwrap();
    let m = root.memory();
    rc_memory::synth::seed_cells(m, NOW).unwrap();
    rc_memory::synth::seed(m, NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let v = probe_view(m, None, Q, NOW).unwrap();
    let (ids, cells) = shape(&v);
    (root, vec![(ids.concat(), cells)])
}

/// 把活库的 `memory.db` 当作快照复制出去（阶段 2 才有真正的快照同步；这里只要"同一份字节两个句柄"）。
/// 复制前必须 checkpoint——这一句是真实同步路径将来也要做的事（阶段 2 的快照导出同样要 VACUUM INTO），
/// 不是测试的补丁。偏离 brief：原文在 checkpoint **之前**还多复制了一次同一目标文件，
/// 那一次复制的内容随后就被覆盖，等于什么都没断，这里删掉它、只留 checkpoint 之后的那一次。
fn checkpoint_and_copy(live_root: &Root, snap: &std::path::Path) {
    live_root
        .memory()
        .conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .unwrap();
    std::fs::copy(live_root.data_dir().join("memory.db"), snap).unwrap();
}

#[test]
fn 同一份副本与活体逐_id_同序同格子() {
    let live_dir = tempfile::tempdir().unwrap();
    let (live_root, live) = build_live(live_dir.path());
    let snap_dir = tempfile::tempdir().unwrap();
    let snap = snap_dir.path().join("memory.db");
    checkpoint_and_copy(&live_root, &snap);
    let agent_dir = tempfile::tempdir().unwrap();
    let ar = AgentRoot::open_replica(agent_dir.path(), &snap).unwrap();
    let v = probe_view(ar.memory(), None, Q, NOW).expect("副本必须可检索（索引层就在同一个文件里）");
    let got = shape(&v);
    assert_eq!(live[0].0, got.0.concat(), "副本与活体的注入顺序不同");
    assert_eq!(live[0].1, got.1.clone(), "副本与活体的沉默计数不同");
    // 偏离 brief（加强，不是放宽）：聚合读数相等不足以说"同一个闸门决定"——
    // 计数只报五格各几条，顺序只报注入了谁。逐行对齐 `(句柄, 形态, 格子)` 才是本档的落点。
    let want_rows = decided_rows(live_root.memory());
    let got_rows = decided_rows(ar.memory());
    assert_eq!(want_rows.len(), got_rows.len(), "副本与活体的候选池行数不同");
    assert_eq!(want_rows, got_rows, "副本与活体的逐行闸门决定不同");
    assert!(!want_rows.is_empty(), "两边都是空池 ⇒ 上面两条相等是空的");
    // R33（非沉默守卫）：`want_rows` 来自 `rank()` 的候选池，而 `rank` 一个开关都不判 ⇒
    // "两边都把每一格判成 Silent"时上面四条**一起绿**（forms 两边都空、id 串两边都是空串、
    // 五格计数两边都是 [0,0,0,0,0]）。"同一个闸门决定"不许由"两边都不开口"兑现，
    // 所以这里要求池子里至少有一行真的开了口。载荷只有形态，没有正文（守卫 12）。
    assert!(
        want_rows.iter().any(|r| r.1 != Form::Silent),
        "两边把 {} 行全判成 Silent ⇒ 对拍没有断过任何东西",
        want_rows.len()
    );
}

#[test]
fn 同一份副本连跑二十次逐位相同() {
    let live_dir = tempfile::tempdir().unwrap();
    let (live_root, _) = build_live(live_dir.path());
    let snap_dir = tempfile::tempdir().unwrap();
    let snap = snap_dir.path().join("memory.db");
    checkpoint_and_copy(&live_root, &snap);
    let agent_dir = tempfile::tempdir().unwrap();
    let ar = AgentRoot::open_replica(agent_dir.path(), &snap).unwrap();
    // R33（换比的对象）：原文比 `shape` = `(注入 id 串, 五格计数)`，两个都是**聚合**读数——
    // 它说不出"哪一行落在哪一格"，于是"某行在两次运行之间从弱锚漂到低分、而计数恰好互相抵掉"
    // 这一类并列未消解是看不见的。逐行的 `(句柄, 形态, 格子)` 才是这个断言想说的东西。
    let want = decided_rows(ar.memory());
    for i in 0..20 {
        assert_eq!(want, decided_rows(ar.memory()), "第 {i} 次与第一次不同 ⇒ 存在未消解的并列");
    }
}

#[test]
fn 副本只读_探针的冷却写必须响亮失败() {
    // V4 + V7 的联合后果：唯一在读路径上写列的那一次（冷却戳）在副本上必须失败，
    // 否则"副本"只是名字，节点会把自己的注入历史写进一份本该只读的数据里。
    let live_dir = tempfile::tempdir().unwrap();
    let (live_root, _) = build_live(live_dir.path());
    let snap = live_dir.path().join("snap.db");
    checkpoint_and_copy(&live_root, &snap);
    let agent_dir = tempfile::tempdir().unwrap();
    let ar = AgentRoot::open_replica(agent_dir.path(), &snap).unwrap();
    let k = {
        let root = Root::open(live_dir.path()).unwrap();
        rc_memory::reader::facts(root.memory()).unwrap().remove(0).key
    };
    let err = rc_memory::bump_cooldown(ar.memory(), &[k], NOW, rc_memory::COOLDOWN_MS)
        .expect_err("副本上写冷却必须失败");
    let s = format!("{err:?}");
    assert!(s.contains("read-only") || s.contains("ReadOnly"), "报错形状不像只读：{s}");
    // 偏离 brief（加强）：`{:?}` 是变体名，运维真正看到的是 `Display`。
    // 这一半断的是"回执里点名了只读副本"，不是靠 SQLite 的英文errmsg撞运气。
    assert!(err.to_string().contains("read-only"),
        "Display 没说明只读，运维面读不出为什么失败：{err}");
}

#[test]
fn 沉默五格的名字在导出面必须互不相同() {
    // §10 阈值侧③的渲染那一半：四格混成一格即红，所以先断分型字符串两两不同。
    let mut slugs: Vec<String> = GateCell::ALL.iter().map(|c| c.slug()).collect();
    slugs.sort();
    let n = slugs.len();
    slugs.dedup();
    assert_eq!(slugs.len(), n, "格子分型有重名：{slugs:?}");
    assert_eq!(slugs, ["cooldown", "low_score", "rejected", "stale", "weak_anchor"]);
}
