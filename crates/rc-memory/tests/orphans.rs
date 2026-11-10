//! 段 0 的挂点测试。依据行与边都直接 insert（`ref_kind != 'turn'` 的 provenance 行 +
//! `relationship` 行），不走 rc-derive——段 0 的语义不依赖产生方式，只依赖读侧形状。
//!
//! use 段一次写对：`cargo clippy --workspace --all-targets` 的零告警是本计划的收口门，
//! 一个未用的 import 就是红（`attest_supported` / `resolve_retirement_supported` 这两个
//! 占位名字因此**不写**——本文件一行都不引用它们）。

use rc_memory::identity::compute_edge_id;
use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::orphans::suppress_orphans;
use rc_memory::writers_c::retire;
use rc_memory::writers_x::{put_card, put_chains, put_edges, put_entity, BatchCtx, ChainContent,
                            EdgeContent, EntityContent, FactContent};
use rc_memory::{MemKey, Provenance};
use rc_store::MemoryHandle;

/// 真库小帮手。`MemoryHandle` 只能在组合根（`Root::open`）构造，而 `TempDir` 一掉就把库文件
/// 删了 ⇒ 两者必须成对返回（`crates/rc-index/tests/rebuild.rs` 的 `memory_handle` 同形；
/// tests/*.rs 互不引用，所以这四行在本文件里就地写一遍）。
fn memory_handle() -> (tempfile::TempDir, MemoryHandle) {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    (dir, root.into_memory())
}

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "p-synthetic".into() } }

/// 实体行：`entity` 表只有 `kind`/`title` 两列内容（没有正文列），所以形参叫 title。
/// 返回行 id = `put_*` 的 `MemKey.id`——**不自己算 id**：内容寻址那把尺子是 `compute_mem_id`
/// 的五键形状（表 + kind + title + body + 归属），手搓一份只会算出与写入侧不同的 id。
fn make_entity(m: &MemoryHandle, title: &str, turn_uid: &str, now: i64) -> String {
    put_entity(m, &EntityContent { kind: "concept".into(), title: title.into(),
                                   scope: scope(), aliases: vec![] },
               &[Provenance { turn_uid: turn_uid.into(), anchor_role: AnchorRole::Human }],
               now).unwrap().id
}

fn make_card(m: &MemoryHandle, body: &str, turn_uid: &str, now: i64) -> String {
    put_card(m, &FactContent { kind: "decision".into(), title: "t".into(), body: body.into(),
                               scope: scope(), valid_to_ms: None },
             &[Provenance { turn_uid: turn_uid.into(), anchor_role: AnchorRole::Human }],
             now).unwrap().id
}

/// 直接 insert 一行 provenance（ref_kind != 'turn'），不走 writers_x。
/// 列序与 `writers_x::insert_derived_provenance` 相比**省略 `ord`/`role` 两格**（`role` 走 DEFAULT
/// `'basis'`、`ord` 是可空列 ⇒ 省略即 NULL），其余同形（那一格是生产侧唯一的出处写入点）。
/// 原句写的是"逐字同形"——批次③ 段 B 给那个助手加了 `role`/`ord` 两格之后它不再逐字同形，按实测改实。
fn insert_provenance(m: &MemoryHandle, citing_table: &str, citing_id: &str,
                     ref_kind: &str, target_id: &str) {
    m.conn().execute(
        "INSERT INTO provenance(mem_table, mem_id, turn_uid, anchor_role, ref_kind)
         VALUES(?1,?2,?3,'human',?4)",
        rusqlite::params![citing_table, citing_id, target_id, ref_kind]
    ).unwrap();
}

/// 直接 insert 一行 relationship，返回它的 id（= `compute_edge_id`，与 `put_edges` 同尺）。
/// 13 列全给：`source_agent` / `device_id` / `norm_version` / `created_at_ms` 都是 NOT NULL，
/// 而这段夹具的合成署名值与 `BatchCtx::synthetic()` 逐字同串（守卫：署名值不许漂）。
/// `norm_version` 走 `?4` 而不写字面量 `1`：与 `put_edges`/`put_timeline_rows` 同一把尺，
/// 常量升版时这张新表不会悄悄停在旧值（那会让"这段夹具写的是当前规范版"这句话变成假话）。
fn insert_relationship(m: &MemoryHandle, from_id: &str, to_id: &str) -> String {
    let id = compute_edge_id(from_id, "part_of", to_id);
    m.conn().execute(
        "INSERT INTO fact_link(id, from_id, to_id, relation, basis, evidence_json, decided_by,
                               scope_kind, scope_key, world_id, source_agent, device_id,
                               created_at_ms, norm_version)
         VALUES(?1,'anchor:'||?2,'anchor:'||?3,'part_of','','[]',NULL,'project','p-synthetic',
                NULL,'synthetic-agent','dev-synthetic-1',100,?4)",
        rusqlite::params![id, from_id, to_id, rc_store::capability::NORM_VERSION as i64]
    ).unwrap();
    id
}

fn retirement_count(m: &MemoryHandle, table: &str, id: &str) -> i64 {
    m.conn().query_row(
        "SELECT COUNT(*) FROM retirement WHERE mem_table=?1 AND mem_id=?2 AND resolved_at_ms IS NULL",
        rusqlite::params![table, id], |r| r.get(0)
    ).unwrap()
}

/// 退场账的落账时刻。F8② 的判据载体：**它必须是第一次退场那一轮的 `now_ms`**，
/// 而不是最后一次被扫到的那一轮（`retire` 的 `ON CONFLICT DO UPDATE` 会改写 `created_at_ms`）。
fn retirement_created_at_ms(m: &MemoryHandle, table: &str, id: &str) -> i64 {
    m.conn().query_row(
        "SELECT created_at_ms FROM retirement WHERE mem_table=?1 AND mem_id=?2",
        rusqlite::params![table, id], |r| r.get(0)
    ).unwrap()
}

// ===== 族①：三死法 × 引用行 =====

#[test]
fn provenance_citing_retired_target_gets_retired() {
    let (_dir, m) = memory_handle();
    let target = make_entity(&m, "alpha", "turn-1", 100);
    let citation = make_card(&m, "cites-alpha", "turn-1", 100);
    insert_provenance(&m, "card", &citation, "anchor", &target);

    // 让 target 退场（写退场账）
    retire(&m, &MemKey { table: MemTable::Anchor, id: target.clone() }, "manual", 200).unwrap();

    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_provenance, 1, "族① 应识别 target 退场");
    assert_eq!(retirement_count(&m, "card", &citation), 1, "引用行连带退场");
}

#[test]
fn provenance_citing_rejected_target_gets_retired() {
    let (_dir, m) = memory_handle();
    let target = make_entity(&m, "beta", "turn-1", 100);
    let citation = make_card(&m, "cites-beta", "turn-1", 100);
    insert_provenance(&m, "card", &citation, "anchor", &target);

    // 让 target 被否决（写 attestation.stance='rejected'；`attest` 走 writers_a，
    // 但那一支要求锚轮次在册，跨库身份值在这里造不出 ⇒ 直接写账本表）
    m.conn().execute(
        "INSERT INTO attestation(mem_table, mem_id, stance, actor, reason, created_at_ms)
         VALUES('anchor',?1,'rejected','agent','no',100)",
        rusqlite::params![target]
    ).unwrap();

    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_provenance, 1, "族① 应识别 target 被否决");
    assert_eq!(retirement_count(&m, "card", &citation), 1, "引用行连带退场");
}

#[test]
fn provenance_citing_superseded_target_gets_retired() {
    let (_dir, m) = memory_handle();
    // **这一条的被顶替目标必须是 card，不能是 entity**：`entity` 表没有 `superseded_by` 列，
    // 而 `reader::superseded_pred` 对实体恒真（"顶替本来就只发生在有行的地方"是它自己写的），
    // 所以"实体被顶替"这一形状在库里造不出来——拿它当目标 ⇒ 三支判死恒不亮、这条用例恒绿于
    // 移除顶替那一支（空过）。卡表有那一列，三死法因此都测得到。
    let target = make_card(&m, "gamma", "turn-1", 100);
    let citation = make_card(&m, "cites-gamma", "turn-1", 100);
    insert_provenance(&m, "card", &citation, "card", &target);

    let newer = make_card(&m, "gamma-v2", "turn-2", 150);
    m.conn().execute("UPDATE card SET superseded_by=?1 WHERE id=?2",
                     rusqlite::params![newer, target]).unwrap();

    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_provenance, 1, "族① 应识别 target 被顶替");
    assert_eq!(retirement_count(&m, "card", &citation), 1, "引用行连带退场");
}

// ===== 族②：任一端死 × 边 =====

#[test]
fn edge_with_dead_from_endpoint_gets_retired() {
    let (_dir, m) = memory_handle();
    let from = make_entity(&m, "from", "turn-1", 100);
    let to = make_entity(&m, "to", "turn-1", 100);
    let edge = insert_relationship(&m, &from, &to);

    // from 退场
    retire(&m, &MemKey { table: MemTable::Anchor, id: from.clone() }, "manual", 200).unwrap();

    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_edges, 1, "族② 应识别 from 端退场");
    assert_eq!(retirement_count(&m, "fact_link", &edge), 1, "边连带退场");
}

#[test]
fn edge_with_dead_to_endpoint_gets_retired() {
    let (_dir, m) = memory_handle();
    let from = make_entity(&m, "from-2", "turn-1", 100);
    let to = make_entity(&m, "to-2", "turn-1", 100);
    let edge = insert_relationship(&m, &from, &to);

    retire(&m, &MemKey { table: MemTable::Anchor, id: to.clone() }, "manual", 200).unwrap();

    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_edges, 1, "族② 应识别 to 端退场");
    assert_eq!(retirement_count(&m, "fact_link", &edge), 1, "边连带退场");
}

/// 族② 的 **Missing 端点**这一支：判边死的条件是"from 端或 to 端 `target_status` 不是
/// `Alive`"，而 `target_status` 在**行根本不存在**时返回 `Missing`（fail-closed）⇒ 端点行
/// 不存在（幽灵 id）时边**同样退场**。这一支此前无用例：上面两条只钉了 `Dead`（端点行在、
/// 死判据亮）。红形（把 `target_status` 的 missing 支改成 Alive 之后）：`from_edges` 报 **0**
/// —— 幽灵端点被判成活、边被 `continue` 跳过、账本零行，而计数也不涨 ⇒ 判退场的那一格静默失效。
///
/// 幽灵 id 与 `missing_target_is_treated_as_dead_and_counted` 用同一个（64 个 0）：内容寻址是
/// 公共 API，"指针指向一张账零行的表"是走得到的形状（`dead_by_id_expr` 的注释同此理由）。
/// 另一个端点给一条真活实体 ⇒ 边退场的**唯一**理由就是 from 端不存在。
///
/// 末行那句 `missing_targets == 0` 是防串格：`SuppressReport::missing_targets` 按其自身文档
/// 是 `from_provenance` 的子集，只数族① 的 missing 依据行；族② 的 Missing 端点**不**进那一格
/// （边自己的 provenance 行被族① 的 `mem_table <> 'relationship'` 谓词排掉了）。
#[test]
fn edge_with_missing_endpoint_gets_retired() {
    let (_dir, m) = memory_handle();
    let ghost = "0".repeat(64); // 库里不存在的一行
    let to = make_entity(&m, "missing-endpoint-to", "turn-1", 100);
    let edge = insert_relationship(&m, &ghost, &to);

    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_edges, 1, "族② 应识别 from 端行不存在（Missing ⇒ fail-closed 退场）");
    assert_eq!(retirement_count(&m, "fact_link", &edge), 1, "端点不存在的边连带退场");
    assert_eq!(r.missing_targets, 0,
        "missing_targets 是族① 的子集：族② 的 Missing 端点不进那一格");
}

// ===== 族①/族② 的分工：边自己的 provenance 行不许被族① 再记一次 =====

/// 这一条必须**走真写入路径**（`writers_x::put_edges`）而不能手搓 `relationship` 行：
/// 手搓出来的那条边不带配套 provenance 行，于是"边有依据行"这个形状在库里根本不存在，
/// 缺陷就不红——`put_edges` 给每条新边插的那条 `provenance(mem_table='relationship',
/// mem_id=<边 id>, turn_uid=<from 端行 id>, ref_kind=<from 端表>)` 才是被族①捞到的真凶。
///
/// 红形（改代码前）：`from_edges` 报 1 而 `from_provenance` 也报 1，可 `retirement` 表里
/// `mem_table='relationship'` 的行**一条都没有**——`retire_citing_row` 要
/// `MemTable::parse("relationship")`，而边不可指认句柄（裁定 3）⇒ `None` ⇒ `return Ok(())`，
/// 账本 0 行计数 +1。同一件事（from 端死）被族② 与族① 各记一遍，而族① 那一遍什么也没写。
///
/// 任务后 `put_edges` 插的是三族行（from 端、to 端与每条依据各一行），族① 捞到的仍是
/// 它的 from 端 provenance 行（to 端与依据行同样进表，但族① 的判死键仍只看 from 端）。
#[test]
fn edge_written_by_put_edges_is_counted_once_by_family_two_only() {
    let (_dir, m) = memory_handle();
    let from = make_entity(&m, "edge-from", "turn-1", 100);
    let to = make_entity(&m, "edge-to", "turn-1", 100);
    // P 独占列只从 `BatchCtx` 进写入口（守卫 11），所以署名值不写在 `EdgeContent` 上；
    // 这两个字面量与 `rc_derive::derive_pass` 的那两个常量逐字同串（守卫：署名值不许漂）。
    let ctx = BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() };
    let edge = compute_edge_id(&from, "part_of", &to);
    let n = put_edges(&m, &[EdgeContent { from_id: from.clone(), to_id: to.clone(),
                                        kind: "part_of".into(), reason: "".into(),
                                        scope: scope(), basis_ids: vec![] }],
                     &ctx, 100).unwrap();
    assert_eq!(n, 1, "真写入路径落了一条边（连带它那族 provenance 行）");

    // 让 from 端死 ⇒ 族① 与族② 的判死谓词**逐字相同**（`ref_kind` 就是 from 端表前缀、
    // `turn_uid` 就是 `from_id`）⇒ 这正是"同一件事扫两遍"的那个形状。
    retire(&m, &MemKey { table: MemTable::Anchor, id: from.clone() }, "manual", 200).unwrap();

    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_edges, 1, "族② 认这条边（from 端退场）");
    assert_eq!(r.from_provenance, 0,
        "它的那族 provenance 行归族② 管：族① 捞到它却写不出账本（边不可指认句柄），不许计数");
    assert_eq!(retirement_count(&m, "fact_link", &edge), 1,
        "计数与账本必须对得上：这条边只由族② 记一笔账");
}

/// `relationship` 的 `from_kind` / `to_kind` **没有 CHECK**（DDL 已核：那张表只有 `kind` 一列
/// 带 `CHECK(kind IN ('part_of','depends_on','owns','located_in','role'))`）⇒ 残值是真能落库的
/// 形状，不是一个假想。DDL 那条建表语句的注释点名"T6 段 0 已写明这条下界"，这一条就是那个
/// 下界的用例：残值走低格、跳过、不上抛（架构 §6 连带④ 同律）。
#[test]
fn edge_with_unparseable_endpoint_kind_is_skipped_without_error() {
    let (_dir, m) = memory_handle();
    let from = make_entity(&m, "residual-from", "turn-1", 100);
    let to = make_entity(&m, "residual-to", "turn-1", 100);
    let id = compute_edge_id(&from, "part_of", &to);
    // `from_kind` 塞一个四表字面量之外的词（模拟 schema 升级留下的旧字符串）
    m.conn().execute(
        "INSERT INTO fact_link(id, from_id, to_id, relation, basis, evidence_json, decided_by,
                               scope_kind, scope_key, world_id, source_agent, device_id,
                               created_at_ms, norm_version)
         VALUES(?1,'legacy_scope:'||?2,'anchor:'||?3,'part_of','','[]',NULL,'project','p-synthetic',
                NULL,'synthetic-agent','dev-synthetic-1',100,?4)",
        rusqlite::params![id, from, to, rc_store::capability::NORM_VERSION as i64]
    ).unwrap();
    // 两端都退场：残值支若不生效，这里会判 dead 然后 `retire_edge` ⇒ `from_edges == 1`。
    retire(&m, &MemKey { table: MemTable::Anchor, id: from.clone() }, "manual", 200).unwrap();
    retire(&m, &MemKey { table: MemTable::Anchor, id: to.clone() }, "manual", 200).unwrap();

    // 不报错（残值不上抛）且一条边都不记
    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_edges, 0, "from_kind 认不出 MemTable ⇒ 跳过，不判死、不记账");
    assert_eq!(retirement_count(&m, "fact_link", &id), 0, "跳过 = 不落退场账");
}

// ===== 幂等 / missing / 时序 =====

#[test]
fn running_twice_does_not_double_count_or_add_rows() {
    let (_dir, m) = memory_handle();
    let target = make_entity(&m, "idemp", "turn-1", 100);
    let citation = make_card(&m, "idemp-citation", "turn-1", 100);
    insert_provenance(&m, "card", &citation, "anchor", &target);
    retire(&m, &MemKey { table: MemTable::Anchor, id: target.clone() }, "manual", 200).unwrap();

    let r1 = suppress_orphans(&m, 300).unwrap();
    let r2 = suppress_orphans(&m, 400).unwrap();
    assert_eq!(r1.from_provenance, 1);
    // 第二轮是 **0**：段 0 的族① 带 `NOT EXISTS (… retirement …)`，这条引用行第一轮就拿到了
    // 未解决的退场账 ⇒ 第二轮不再进扫描集合。这一格数的是**变化**不是状态。
    // 账本仍然只增一条（`ON CONFLICT` 重写、不长行）——**下面那两格才是幂等的判据**。
    assert_eq!(r2.from_provenance, 0,
        "族① 的过滤让已退场的引用行退出扫描集合：第二轮不该再触发一次退场");
    assert_eq!(retirement_count(&m, "card", &citation), 1,
        "幂等保证的是账本不增行：跑两遍之后仍然只有一条退场账");
    // F8②：过滤顺带把退场时刻冻结住。`retire` 的 `ON CONFLICT DO UPDATE` 会把 `created_at_ms`
    // 改写成当轮 `now_ms`，而第二轮根本不再重发 ⇒ 停在 300，**不是** 400。
    assert_eq!(retirement_created_at_ms(&m, "card", &citation), 300,
        "退场时刻是第一次退场那一轮（300），不是最后一次被扫到的那一轮（400）");
}

#[test]
fn running_twice_leaves_a_retired_edge_out_of_family_two() {
    // 族② 的对称格。上面那条只覆盖族① ⇒ **只撤族② 的过滤全档照旧绿**（真空过的守卫）：
    // 族② 原有那七条用例全是单轮形状，没有一条跑过第二轮。
    let (_dir, m) = memory_handle();
    let from = make_entity(&m, "edge-idem-from", "turn-1", 100);
    let to = make_entity(&m, "edge-idem-to", "turn-1", 100);
    let edge = insert_relationship(&m, &from, &to);
    retire(&m, &MemKey { table: MemTable::Anchor, id: from.clone() }, "manual", 200).unwrap();

    let r1 = suppress_orphans(&m, 300).unwrap();
    let r2 = suppress_orphans(&m, 400).unwrap();
    assert_eq!(r1.from_edges, 1, "from 端退场 ⇒ 这条边本轮退场");
    assert_eq!(r2.from_edges, 0,
        "族② 与族① 同律：这条边自己已有未解决的退场账，第二轮就不该再扫到它");
    assert_eq!(retirement_count(&m, "fact_link", &edge), 1);
    assert_eq!(retirement_created_at_ms(&m, "fact_link", &edge), 300,
        "边这一支的退场时刻同样冻结在第一轮");
}

#[test]
fn missing_target_is_treated_as_dead_and_counted() {
    let (_dir, m) = memory_handle();
    let citation = make_card(&m, "missing-citation", "turn-1", 100);
    // 引用一个不存在的实体——依据行 missing（id 是任意指针，不必算得出：内容寻址是公共 API，
    // "一张账、零行"是走得到的形状，见 `dead_by_id_expr` 的注释）
    insert_provenance(&m, "card", &citation, "anchor", "0000000000000000000000000000000000000000000000000000000000000000");

    let r = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r.from_provenance, 1, "missing ⇒ fail-closed 退场");
    assert_eq!(r.missing_targets, 1, "missing 单独计数");
    assert_eq!(retirement_count(&m, "card", &citation), 1);
}

#[test]
fn suppress_orphans_picks_up_rows_from_previous_round() {
    let (_dir, m) = memory_handle();
    let target = make_entity(&m, "stale", "turn-1", 100);
    let citation = make_card(&m, "stale-citation", "turn-1", 100);
    insert_provenance(&m, "card", &citation, "anchor", &target);

    // 第一轮：target 仍活 ⇒ 不动
    let r1 = suppress_orphans(&m, 100).unwrap();
    assert_eq!(r1.from_provenance, 0);

    // 中间：target 退场（别的写入路径）
    retire(&m, &MemKey { table: MemTable::Anchor, id: target.clone() }, "manual", 200).unwrap();

    // 第二轮：检测到状态变化
    let r2 = suppress_orphans(&m, 300).unwrap();
    assert_eq!(r2.from_provenance, 1);
    assert_eq!(retirement_count(&m, "card", &citation), 1);
}

// ===== 批次③ 段 C-1：因果链的连坐覆盖面（新覆盖面，不是新机制） =====

/// 库里唯一那条因果链的头行 id。本组夹具一条链 ⇒ 一行 `event(kind='causal')`。
fn causal_head(m: &MemoryHandle) -> String {
    m.conn().query_row("SELECT id FROM card WHERE kind='causal'", [], |r| r.get(0)).unwrap()
}

/// R6（工单 §5-5 · 段 C-1 的新覆盖面）：一条链 = 1 行链头 + N 环节 + M 依据，这些出处行**逐行**
/// 落进族①（`ref_kind != 'turn'`；链头的 `mem_table='event'` 不被那句 `<> 'relationship'` 挡）。
/// 某个环节的目标退场 ⇒ 既有连带抑制把**链头**一并退场——这不是新机制，是段 B 之后新出现的覆盖面。
///
/// **计数形状是实测的、不是推的**：本条把链头的**两个环节**（甲、乙）的目标都退场、依据丙留着，
/// 跑一次段 0。读回来的 `from_provenance` 见下面断言（跑之前并不知道，是段 B 之后才有的新形状 ⇒
/// 先把数跑出来、再写死）。族① 那个计数是**按出处行逐行抬**还是**按链头抬**，就是这一条要钉死的：
/// 若是按行 ⇒ 两个环节各一次；若是按链头 ⇒ 只有 1。账本（`retirement`）无论如何都只有 1 行，
/// 因为 `retire` 的 `ON CONFLICT` 对同一行不长第二行——**账本行与计数不是一回事**，这里要分开看。
///
/// 变异形（本条实测过）：给族① 的扫描谓词加一条 `AND p.mem_table <> 'event'`（= 有人给链头开了后门）
/// ⇒ 链头的出处行整体不进扫描面 ⇒ 链头不退场、`from_provenance` 归零 ⇒ R6 红（R7 同红，见那里）。
#[test]
fn 因果链头的两个环节退场时链头连坐退场_计数按出处行逐行抬() {
    let (_dir, m) = memory_handle();
    let a = make_card(&m, "环节甲", "turn-a", 100);
    let b = make_card(&m, "环节乙", "turn-b", 100);
    let c = make_card(&m, "依据丙", "turn-c", 100);
    let ctx = BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() };
    put_chains(&m, &[ChainContent { mechanism: "甲顶掉乙，丙是它凭什么".into(),
                                    link_ids: vec![a.clone(), b.clone()],
                                    basis_ids: vec![c.clone()],
                                    causal_type: "causes".into(), scope: scope() }], &ctx, 200).unwrap();
    let head = causal_head(&m);
    assert_eq!(retirement_count(&m, "card", &head), 0, "夹具前提：跑段 0 之前链头还活着");

    // 两个环节的目标都退场（甲、乙）；依据丙不动。三者都是**真退场**（行在、死判据亮），不是 Missing。
    retire(&m, &MemKey { table: MemTable::Card, id: a.clone() }, "manual", 300).unwrap();
    retire(&m, &MemKey { table: MemTable::Card, id: b.clone() }, "manual", 300).unwrap();

    let r = suppress_orphans(&m, 400).unwrap();
    // 实测数（跑之前未知、跑出来写死）：链头两条环节出处退场 ⇒ 族① 按出处行逐行抬，读回 2。
    assert_eq!(r.from_provenance, 2,
        "实测：同一个链头的两条环节出处退场 ⇒ 族① 按**出处行数**记（2），不是按**链头**记（1）");
    assert_eq!(r.missing_targets, 0, "甲乙都在、只是退场 ⇒ Dead 不是 Missing");
    assert_eq!(r.from_edges, 0, "本夹具没有边 ⇒ 族② 一格不涨（别把连坐数算到边头上）");
    assert_eq!(retirement_count(&m, "card", &head), 1,
        "账本只有一条：两条出处把同一个链头各触发一次，ON CONFLICT 不长第二行 ⇒ 计数与账本分开");
}

/// R7（工单 §5-5 的第二形）：**依据退场**。与 R6 同族（都落 `ref_kind != 'turn'`）、连坐形状相同
/// （链头一并退场），但这一形只有**一条**出处退场 ⇒ 计数与 R6 不同形（R6 两条 ⇒ 见 R6 的实测数，
/// 本条一条 ⇒ 见下面实测数）。两条形合起来才钉死「族① 按出处行逐行抬、不按链头抬」——单看任一条
/// 都分不清到底按行还是按链头：R7 若也只报 1，那既可能是"按行(只 1 行)"也可能是"按链头(1 个)"，
/// 正是 R6 那条报 2 把它逼成"按行"。所以 R6 与 R7 是**同一格的两面**，不是两个独立判据。
///
/// 变异形同 R6（`AND p.mem_table <> 'event'`）⇒ 本条与 R6 一起红；两者同红时按工单点名它们是同一格
/// 的两面（一个链头，环节退场与依据退场各测一半）。
#[test]
fn 因果链头的依据退场时链头同样连坐_计数与环节形不同形() {
    let (_dir, m) = memory_handle();
    let a = make_card(&m, "环节甲", "turn-a", 100);
    let b = make_card(&m, "环节乙", "turn-b", 100);
    let c = make_card(&m, "依据丙", "turn-c", 100);
    let ctx = BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() };
    put_chains(&m, &[ChainContent { mechanism: "甲顶掉乙，丙是它凭什么".into(),
                                    link_ids: vec![a.clone(), b.clone()],
                                    basis_ids: vec![c.clone()],
                                    causal_type: "causes".into(), scope: scope() }], &ctx, 200).unwrap();
    let head = causal_head(&m);

    // 只有依据的目标退场（丙）；环节甲乙都不动。
    retire(&m, &MemKey { table: MemTable::Card, id: c.clone() }, "manual", 300).unwrap();

    let r = suppress_orphans(&m, 400).unwrap();
    // 实测数：只有一条依据出处退场 ⇒ 族① 记 1。与 R6 的 2 对照，才说明它是按行不是按链头。
    assert_eq!(r.from_provenance, 1,
        "实测：链头只有一条依据退场 ⇒ 族① 记 1；R6 里两条环节退场记 2 ⇒ 合起来钉死「按出处行」");
    assert_eq!(r.missing_targets, 0);
    assert_eq!(retirement_count(&m, "card", &head), 1, "依据退场同样把链头连坐退场");
    // 环节甲乙都活 ⇒ 它们那两条出处行没触发退场；链头退场这一格只由丙那一条撑起来。
    assert_eq!(retirement_count(&m, "card", &a), 0, "环节甲自己没死，不该被连坐");
    assert_eq!(retirement_count(&m, "card", &b), 0, "环节乙自己没死，不该被连坐");
}
