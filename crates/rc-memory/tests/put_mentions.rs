//! 批次 M 段 B · mention 写入口 `put_mentions` 的用例（5 条）。
//!
//! 与 `put_chains.rs` 同一条纪律：能进用例的形状都从生产写入口走
//!（`put_entity` / `put_card` / `put_event`），**不手搓 `mention` 行**——手搓等于把被测的那句
//! SQL 换成用例自己的 SQL，生产代码改坏也不会红。
//! 可观察面三格：返回读数（`written` / `unresolved`）、`mention` 表行数、以及每行的
//! `from_kind`/`from_id`/`entity_id` 与四列 P 侧署名。

use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::writers_x::{link_row_mentions, put_card, put_entity, put_event, put_mentions,
                           BatchCtx, EntityContent, FactContent, MentionContent};
use rc_memory::{MemError, Provenance};
use rc_store::MemoryHandle;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-mention".into() } }

/// P 独占列只从 `BatchCtx` 进写入口（守卫 11）。这两个字面量与 `rc_derive::derive_pass`
/// 的那两个常量逐字同串（守卫：署名值不许漂）。
fn ctx() -> BatchCtx { BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() } }

fn prov(seed: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: seed.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

fn entity(m: &MemoryHandle, title: &str, aliases: &[&str], seed: char, s: Scope) -> String {
    put_entity(m, &EntityContent {
        kind: "person".into(), title: title.into(), scope: s,
        aliases: aliases.iter().map(|a| a.to_string()).collect(),
    }, &prov(seed), NOW).unwrap().id
}

fn card(m: &MemoryHandle, title: &str, body: &str, seed: char) -> String {
    put_card(m, &FactContent {
        kind: "decision".into(), title: title.into(), body: body.into(),
        scope: scope(), valid_to_ms: None,
    }, &prov(seed), NOW).unwrap().id
}

fn mention_row_count(m: &MemoryHandle) -> i64 {
    m.conn().query_row("SELECT count(*) FROM memory_mention", [], |r| r.get(0)).unwrap()
}

/// 标题与别名都解析到**同一个实体**；重跑、换名重跑都是 0 新增；落库四列逐字对。
#[test]
fn resolves_by_alias_and_title_and_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let e = entity(m, "张三", &["老张"], 'e', scope());
    let c1 = card(m, "排期", "张三说周五上线", 'c');
    let rows = [MentionContent { row_id: c1.clone(), name: "张三".into() }];
    let out = put_mentions(m, &rows, &ctx(), NOW).unwrap();
    assert_eq!((out.written, out.unresolved), (1, 0), "标题解析到实体 ⇒ 一条新行");
    // 再跑同一调用 ⇒ 撞主键 ⇒ 0（`INSERT OR IGNORE`）
    let again = put_mentions(m, &rows, &ctx(), NOW + 1).unwrap();
    assert_eq!(again.written, 0, "主键 (from_kind, from_id, entity_id) 让重复标注幂等");
    assert_eq!(mention_row_count(m), 1, "重跑不许长出第二行");
    // 换 alias 对同一行 ⇒ 解析到同一 E ⇒ 撞主键 ⇒ 0
    let by_alias = [MentionContent { row_id: c1.clone(), name: "老张".into() }];
    assert_eq!(put_mentions(m, &by_alias, &ctx(), NOW + 2).unwrap().written, 0,
        "标题与别名解析到同一实体 ⇒ 同一主键 ⇒ 不许长出第二行");
    assert_eq!(mention_row_count(m), 1);
    // 行面：from_kind 现算 = card；四列 P 侧署名逐字对
    let row: (String, String, String, String, String, i64) = m.conn().query_row(
        "SELECT mem_table, mem_id, anchor_id, source_agent, device_id, norm_version FROM memory_mention",
        [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap();
    assert_eq!(row.0, "card", "mem_table 必须从行所在表现算");
    assert_eq!(row.1, c1);
    assert_eq!(row.2, e);
    assert_eq!((row.3.as_str(), row.4.as_str()), ("rc-derive", "unknown"),
        "source_agent / device_id 只从 BatchCtx 取（守卫 11）");
    assert_eq!(row.5, rc_store::capability::NORM_VERSION as i64,
        "norm_version 只从常量取，不写字面量");
}

/// 多义名字**落全部匹配**（近名不替模型选）：两个不同 scope 的同名实体 ⇒ 一条 mention 落两行。
#[test]
fn multi_meaning_name_writes_every_match() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let other = Scope { kind: ScopeKind::Project, key: "proj-other".into() };
    let e1 = entity(m, "李四", &[], 'a', scope());
    let e2 = entity(m, "李四", &[], 'b', other);
    assert_ne!(e1, e2, "scope 是 compute_mem_id 的一键 ⇒ 同名不同 scope 必须是两个 id");
    let c = card(m, "排期", "李四说下周上线", 'c');
    let out = put_mentions(m, &[MentionContent { row_id: c, name: "李四".into() }], &ctx(), NOW)
        .unwrap();
    assert_eq!(out.written, 2, "一条 mention 命中两个实体 ⇒ 落两行");
    assert_eq!(out.unresolved, 0);
    assert_eq!(mention_row_count(m), 2, "两个实体各一行：多义名字不许只挑一个");
}

/// 零匹配的名字只计数、不造实体、不写行；同一次调用里能解析的照写。
#[test]
fn unresolved_name_is_counted_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let _e = entity(m, "张三", &[], 'e', scope());
    let c = card(m, "排期", "张三和李四都要上线", 'c');
    let out = put_mentions(m, &[
        MentionContent { row_id: c.clone(), name: "张三".into() },
        MentionContent { row_id: c, name: "李四".into() },
    ], &ctx(), NOW).unwrap();
    assert_eq!((out.written, out.unresolved), (1, 1),
        "能解析的写一条、解析不到的计一条 unresolved");
    assert_eq!(mention_row_count(m), 1, "零匹配不许写行");
    let entities: i64 = m.conn().query_row(
        "SELECT count(*) FROM anchor WHERE canonical='李四'", [], |r| r.get(0)).unwrap();
    assert_eq!(entities, 0, "零匹配不许造实体（合证标注是派生数据，不是实体来源）");
}

/// `from_kind` 是按行所在表现算的：card / event / entity 三张表各一行。
#[test]
fn from_kind_is_computed_from_the_row_table() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let _target = entity(m, "alpha", &[], 'x', scope());
    let e_source = entity(m, "beta", &[], 'y', scope());
    let c = card(m, "排期", "alpha 上线", 'c');
    let ev = put_event(m, &FactContent {
        kind: "meeting".into(), title: "会议".into(), body: "alpha 上线".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('v'), NOW).unwrap().id;
    let out = put_mentions(m, &[
        MentionContent { row_id: c.clone(), name: "alpha".into() },
        MentionContent { row_id: ev.clone(), name: "alpha".into() },
        MentionContent { row_id: e_source.clone(), name: "alpha".into() },
    ], &ctx(), NOW).unwrap();
    assert_eq!((out.written, out.unresolved), (3, 0));
    let mut st = m.conn().prepare("SELECT mem_table, mem_id FROM memory_mention ORDER BY mem_table, mem_id").unwrap();
    let got: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap().map(|x| x.unwrap()).collect();
    // 旧 event 已并入 card ⇒ e_source 是 anchor 表、其余两行同表。
    let mut want = vec![("anchor".to_string(), e_source),
                        ("card".to_string(), c),
                        ("card".to_string(), ev)];
    want.sort();
    assert_eq!(got, want,
        "mem_table 必须跟着 mem_id 所在表走（写在内容形状里的表名是伪造面）");
}

/// 照 `judgement.rs` 的副本形状：只读句柄 ⇒ `MemError::ReadOnly`（不是 sqlite 的文本），且零行。
#[test]
fn refuses_a_read_only_handle_before_touching_sql() {
    let dir = tempfile::tempdir().unwrap();
    let snap = dir.path().join("replica.db");
    let (_e, c) = {
        let root = Root::open(dir.path()).unwrap();
        let m = root.memory();
        let e = entity(m, "张三", &[], 'e', scope());
        let c = card(m, "排期", "张三说周五上线", 'c');
        (e, c)
    };
    std::fs::copy(dir.path().join("memory.db"), &snap).unwrap();
    let ar = rc_store::AgentRoot::open_replica(dir.path(), &snap).unwrap();
    assert!(ar.memory().is_read_only(), "夹具前提破了：副本句柄不是只读的");
    let rows = [MentionContent { row_id: c, name: "张三".into() }];
    let e = put_mentions(ar.memory(), &rows, &ctx(), NOW).unwrap_err();
    assert!(matches!(e, MemError::ReadOnly), "只读句柄要念族名，拿到 {e:?}");
    assert_eq!(mention_row_count(ar.memory()), 0, "拒写之后不许留下半行");
}

/// I6：实体行指向自己的 mention 是纯噪声，写入口直接拦掉并单列 `self_skipped`。
#[test]
fn an_entity_self_mention_is_skipped_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let e = entity(m, "示例主人", &[], 'e', scope());
    let out = put_mentions(m, &[MentionContent { row_id: e.clone(), name: "示例主人".into() }],
                           &ctx(), NOW).unwrap();
    assert_eq!((out.written, out.self_skipped), (0, 1),
        "实体行提到自己 ⇒ 不写行，单列 self_skipped（I6）");
    assert_eq!(mention_row_count(m), 0, "自指 mention 一个字节都不许落库");
    // 对照：实体提到**别的**实体仍然合法。
    let other = entity(m, "alpha", &[], 'a', scope());
    let out2 = put_mentions(m, &[MentionContent { row_id: e, name: "alpha".into() }],
                            &ctx(), NOW).unwrap();
    assert_eq!((out2.written, out2.self_skipped), (1, 0), "跨实体 mention 照写");
    let _ = other;
}

/// I6：别名锚补铸——标题/别名归一后出现在正文里就铸一行；三处"应铸未铸"
/// （`3.0-flash` ↔ `Agnes 3.0 Flash`、`agnes-ai` ↔ `Agnes AI`）由这一支覆盖。
#[test]
fn link_row_mentions_mints_alias_mentions_idempotently() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let agnes = entity(m, "Agnes AI", &["agnes-ai"], 'g', scope());
    let flash = entity(m, "Agnes 3.0 Flash", &[], 'f', scope());
    let c1 = card(m, "视觉绑定", "agnes-ai 提供视觉通道", '1');
    let c2 = card(m, "上下文窗口", "3.0-flash 的窗口是 512K", '2');
    let rows = vec![(c1.clone(), "视觉绑定 agnes-ai 提供视觉通道".to_string()),
                    (c2.clone(), "上下文窗口 3.0-flash 的窗口是 512K".to_string())];
    let n = link_row_mentions(m, &rows, &ctx(), NOW).unwrap();
    assert_eq!(n, 2, "两个正文各命中一个实体（含别名与连字符归一）");
    let mut st = m.conn().prepare(
        "SELECT mem_id, anchor_id FROM memory_mention ORDER BY mem_id, anchor_id").unwrap();
    let got: Vec<(String, String)> = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap().map(|x| x.unwrap()).collect();
    assert!(got.contains(&(c1.clone(), agnes.clone())), "agnes-ai 必须解析到 Agnes AI：{got:?}");
    assert!(got.contains(&(c2.clone(), flash.clone())), "3.0-flash 必须解析到 Agnes 3.0 Flash：{got:?}");
    assert_eq!(link_row_mentions(m, &rows, &ctx(), NOW + 1).unwrap(), 0,
        "补铸幂等：同主键第二次 0 新增");
}
