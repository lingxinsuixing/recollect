use rc_agent::{capabilities, AgentErrorKind, ToolError};
use rc_agent::recall::{recall, RecallFilter, RecallRequest};
use rc_store::{AgentRoot, Root};
use std::path::Path;

const NOW: i64 = 1_700_000_000_000;

/// 四种情形各自造一次：非法输入 / 只读副本要求写 / 索引不可用 / 库为空。
/// 阶段 1 没有网络，所以"未配对"与"中心不可达"两臂由**索引未建**与**副本只读**代理——
/// 分型族必须现在就分得开，阶段 4 加真实网络错误时是加法而不是重新分型（C2 演进规则）。
///
/// **四臂共用一个构造处**（而不是四个测试各写一遍），理由与阶段 0 的 `RejectCode::slug()` 同源：
/// "分型不许合并"这条判据要求四臂同源，否则改了一臂的造法、另三臂还绿着，族就空心了。
/// 每臂各占一个独立目录：共用一份库会互相污染（一臂建过索引，另一臂就测不出"从未建过"）。
fn probe_kind(dir: &Path, case: &str) -> ToolError {
    match case {
        // ① 非法输入：查询是空白 ⇒ Validation。**不许与 Empty 混**：两者都"没结果"，
        //    但要求人做的下一步完全不同（改输入 / 承认库里没东西）。
        "invalid" => RecallRequest::query("   ").unwrap_err(),
        // ② 只读副本要求写 ⇒ Authorization。"拿不到写权限"与"此刻不行"必须分开：
        //    混起来的症状是运维对着只读副本反复重跑 `hub index rebuild`，而它永远不会成功。
        "authorization" => {
            let key = {
                let root = Root::open(dir).unwrap();
                let k = rc_memory::synth::seed(root.memory(), NOW).unwrap().remove(0);
                root.memory().conn().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
                k
            };
            let snap = dir.join("snap.db");
            std::fs::copy(dir.join("memory.db"), &snap).unwrap();
            let home = tempfile::tempdir().unwrap();
            let ar = AgentRoot::open_replica(home.path(), &snap).unwrap();
            rc_memory::bump_cooldown(ar.memory(), &[key], NOW, rc_memory::COOLDOWN_MS)
                // 逐字测试原文写 `.err().expect("副本上写冷却必须失败")`；clippy 的
                // `err_expect` 在 `-D warnings` 下是硬错误（本档第一次让这些用例进得了 lint），
                // 改成 `.unwrap_err()`：期望那句话搬到这里，判据一个没少。
                .unwrap_err()
                .into()
        }
        // ③ 有数据但索引从未建 ⇒ Unavailable，且 hint 必须给出下一条命令（§11 D5：让系统自证问题）。
        "unavailable" => {
            let root = Root::open(dir).unwrap();
            rc_memory::synth::seed(root.memory(), NOW).unwrap();
            rc_agent::surface::probe_via_search(root.memory(), None, "同分合成词甲", NOW)
                .unwrap_err() // 未建索引必须报错（同上：`err_expect` 改 `unwrap_err`）
        }
        // ④ 索引正常、查询合法、库里没有 ⇒ Empty。
        "empty" => {
            let root = Root::open(dir).unwrap();
            rc_index::rebuild(root.memory(), NOW).unwrap();
            recall(
                root.memory(),
                &RecallRequest::query("同分合成词甲").unwrap(),
                &RecallFilter::empty(),
                NOW,
            )
            .unwrap_err() // 空库召回必须是 Empty 分型不是 Ok（同上）
        }
        other => panic!("未知分型用例 {other}"),
    }
}

fn directories() -> Vec<tempfile::TempDir> {
    (0..4).map(|_| tempfile::tempdir().unwrap()).collect()
}

#[test]
fn 四类分型两两不同且断的是字段() {
    // 不造四个真错误，先断类型层的性质：四个 slug 两两不同、serde 出来是 snake_case 字符串。
    // 这一条存在是因为另一半（"四种情形真的映射到四种分型"）需要一个能造出四情形的库，见下面四条。
    let slugs: Vec<String> = AgentErrorKind::ALL.iter().map(|k| k.slug()).collect();
    let mut sorted = slugs.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), AgentErrorKind::ALL.len(), "分型重名：{slugs:?}");
    assert_eq!(sorted, ["authorization", "empty", "unavailable", "validation"]);
}

#[test]
fn 情形一_空查询是_validation_不是_empty() {
    // 把这两类混起来是最好写的错法：两者都"没结果"。但它们要求人做完全不同的下一步
    // （改输入 vs 承认库里没东西），所以 §10 点名要分。
    let ds = directories();
    let err = probe_kind(ds[0].path(), "invalid");
    assert_eq!(err.kind, AgentErrorKind::Validation, "{err:?}");
    assert_ne!(err.kind, AgentErrorKind::Empty, "Validation 与 Empty 被合并");
    assert!(!err.hint.is_empty(), "分型有了但没给动作，等于只报了个类别");
}

#[test]
fn 情形二_只读副本上的写要求是_authorization_不是_unavailable() {
    let ds = directories();
    let e = probe_kind(ds[1].path(), "authorization");
    assert_eq!(e.kind, AgentErrorKind::Authorization, "{e:?}");
    assert_ne!(e.kind, AgentErrorKind::Unavailable, "两类被合并");
    // 反向的一半：export 在副本上是**允许**的（它只读），所以同一个句柄上必须成功。
    // 缺这一半的话，实现者会把"任何带副本句柄的调用"一律回 Authorization，
    // 症状是"节点上跑不了 export"——那正是 §9 要求节点能自证清白的那条能力。
    let snap = {
        let root = Root::open(ds[1].path()).unwrap();
        rc_memory::synth::seed(root.memory(), NOW).unwrap();
        root.memory().conn().execute_batch("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        let p = ds[1].path().join("snap2.db");
        std::fs::copy(ds[1].path().join("memory.db"), &p).unwrap();
        p
    };
    let home = tempfile::tempdir().unwrap();
    let ar = AgentRoot::open_replica(home.path(), &snap).unwrap();
    rc_agent::export::render_export(ar.memory(), &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .expect("只读副本必须能导出");
}

#[test]
fn 情形三_索引未建是_unavailable_且_hint_给出下一步命令() {
    // 这一族的症状是"结果不变"，而正确行为是明确说"索引没建，跑 hub index rebuild"。
    // 断 hint 里含命令名：这是 §11 D5 那句"让系统自动定位问题"的最小落地。
    let ds = directories();
    let err = probe_kind(ds[2].path(), "unavailable");
    assert_eq!(err.kind, AgentErrorKind::Unavailable, "{err:?}");
    assert!(err.hint.contains("hub index rebuild"), "hint 没给下一步：{}", err.hint);
    assert!(!err.hint.contains('\n'), "hint 必须单行：多行会被日志拆成两条读数");
}

#[test]
fn 情形四_库为空是_empty_且与_unavailable_是两个码() {
    let ds = directories();
    let empty = probe_kind(ds[3].path(), "empty");
    assert_eq!(empty.kind, AgentErrorKind::Empty, "{empty:?}");
    // 分型对但 code 相同 = 机器读的那一半被合并了（宿主按 code 分支，不看文案）。
    let un = probe_kind(ds[2].path(), "unavailable");
    assert_ne!(empty.code, un.code, "{} 与 {}", empty.code, un.code);
}

#[test]
fn 分型族一旦合并就红_这条是族本身的存在性证明() {
    // 把四臂各造一遍并断**四个 kind 互不相同**。任何两臂被合并（比如有人让空库也回 Validation）当场红。
    let cases: Vec<AgentErrorKind> = vec![
        probe_kind(directories()[0].path(), "invalid").kind,
        probe_kind(directories()[1].path(), "authorization").kind,
        probe_kind(directories()[2].path(), "unavailable").kind,
        probe_kind(directories()[3].path(), "empty").kind,
    ];
    let mut uniq = cases.clone();
    uniq.sort_by_key(|k| k.slug());
    uniq.dedup_by_key(|k| k.slug());
    assert_eq!(uniq.len(), 4, "四类分型被合并了：{cases:?}");
}

#[test]
fn 索引层的三种坏法是三个不同的码_运维动作各不相同() {
    // 裁定 R40：`SE::Index(_)` 那条通配拆成逐变体点名之后，`Sql` / `Store` / `Memory` 三行必须
    // 各给各的读数——把 `Memory` 折进 `index_failed` 的症状是"存储/记忆层坏了却被告知去查索引"
    // （§10 权限④）。这三串与 Step 6 那张 `From<IndexError>` 权威表逐字相同；
    // 两张表**同源**这件事由任务 8 的 `tools_shape.rs::两个来源的分型映射不许漂` 执法，
    // 而那条用例之所以**可能**失败，前提就是这里没写成委托。
    // 那条用例现在的执法面（工单 R50 升级后如实登记）：`kind`/码/hint **三个维度**、
    // `From<IndexError>` 的**八臂逐臂**与 `SE::Index(同一支)` 对撞三元组。本用例自己只读
    // `SE::Index(..)` 一侧的三元组 ⇒ 另一侧那张表单独漂一个字符时，红的是那条对撞用例，不是这里。
    use rc_agent::error::ToolError;
    use rc_index::IndexError;
    use rc_search::SearchError as SE;
    let sql: ToolError = SE::Index(IndexError::Sql("no such table: 消毒后的类别串".into())).into();
    let store: ToolError = SE::Index(IndexError::Store(rc_store::StoreError::Io("io 类别串".into()))).into();
    let memory: ToolError = SE::Index(IndexError::Memory(rc_memory::MemError::ReadOnly)).into();
    assert_eq!((sql.kind, sql.code, sql.hint), (AgentErrorKind::Unavailable, "index_failed", "run: hub index status --data-dir <dir>"));
    assert_eq!((store.kind, store.code, store.hint), (AgentErrorKind::Unavailable, "store_unreachable", "run: doctor"));
    assert_eq!((memory.kind, memory.code, memory.hint), (AgentErrorKind::Unavailable, "memory_failed", "run: doctor"));
    let codes = [sql.code, store.code, memory.code];
    let mut uniq = codes.to_vec();
    uniq.sort_unstable();
    uniq.dedup();
    assert_eq!(uniq.len(), codes.len(), "索引/存储/记忆三层的码被折叠了：{codes:?}");
    let hints = [sql.hint, store.hint, memory.hint];
    let mut uniq_h = hints.to_vec();
    uniq_h.sort_unstable();
    uniq_h.dedup();
    assert_eq!(uniq_h.len(), 2, "next_action 的族被折叠成一条：{hints:?}");
    // 载荷不外泄（守卫 12）：三串的 hint 都是常量动作，一个字也不带下面的错误文本。
    for e in [sql, store, memory] {
        assert!(!e.hint.contains("消毒后的类别串") && !e.hint.contains("io 类别串"), "{e:?}");
        assert!(!e.code.contains(' '), "机读码里不许有句子：{}", e.code);
    }
}

#[test]
fn 空查询在两个门面上是同一个码同一个kind() {
    let via_tool = rc_agent::recall::RecallRequest::query("   ").unwrap_err();
    let via_fn = rc_agent::recall::validate_query("   ").unwrap_err();
    assert_eq!(via_tool, via_fn);
    assert_eq!(via_tool.code, "empty_query");
    assert_eq!(via_tool.kind, rc_agent::AgentErrorKind::Validation);
}

#[test]
fn 沉默格子的清单在_capabilities_里与_gate_同源() {
    // Step 7 证伪第 3 项的载体：`capabilities().silence_cells` 必须**从 `GateCell` 现算**。
    // 把它写成硬编码清单（比如只留 `["low_score","rejected"]`）时这条必须红——否则"格子改名"
    // 或"格子合并"就有一条不会红的路，而 G7 要的是沉默的原因**全部可枚举**。
    let caps = capabilities();
    let expected: Vec<String> = rc_search::gate::GateCell::ALL.iter().map(|c| c.slug()).collect();
    assert_eq!(caps.silence_cells, expected, "agent 面声明的沉默原因与闸门的家不是同一份");
    assert_eq!(caps.silence_cells.len(), rc_search::gate::GateCell::ALL.len(), "五格被削薄了");
    assert_eq!(caps.max_form_lines, rc_search::gate::MAX_FORM_LINES, "上限被抄成了第二份数字");
    assert_eq!(caps.schema_version, rc_agent::SCHEMA_VERSION);
}
