//! 锚合并方案二（D8）：写侧指向行 + 读侧解析的库面证据。
//!
//! 口径分工：CLI 面（`hub anchor merge --from --to`）的证据在 `hub/tests/anchor_merge_redirect_cli.rs`；
//! 读侧消费面（entity 过滤/参与者/名字渲染）的证据在 `tests/anchor_redirect_readers.rs`；
//! 这里证解析函数本身、写侧四动作（指向行/退场章/alias 三格/台账）、零搬运、防环、回滚与变异。
use rc_memory::identity::compute_state_id;
use rc_memory::keys::{AnchorRole, MemKey, MemTable, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::redirect::{broken_redirects, classify_pair, merge_redirect, resolve,
                          RedirectError, RedirectResolve};
use rc_memory::writers_fact::{put_fact_rows, FactWriteItem};
use rc_memory::writers_state::{put_state_rows, StateWriteItem};
use rc_memory::writers_v4::{write_a1_with_owner, A1Anchor, A1Batch};
use rc_memory::{BatchCtx, Provenance};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_001_000_000;
const SRC_MS: i64 = 1_700_000_000_000;
const SCOPE: &str = "proj-redirect";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: SCOPE.into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-redirect".into()),
        owner_agent_id: "agent-redirect".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: "agent-redirect".into(), device_id: "dev-redirect".into() }
}

fn uid(index: usize) -> String {
    format!("{:064x}", 0xc0 + index)
}

/// card id 与生产同一算法（内容寻址；测试里只要求"与插入一致"，用 kind='note'）。
fn compute_card_id(title: &str) -> String {
    rc_memory::compute_mem_id(MemTable::Card, "note", title, "正文", &scope())
}

/// 建一格锚（生产写入口），`stable=false` 落 `provisional`。
fn anchor(m: &MemoryHandle, kind: &str, canonical: &str, aliases: &[&str], stable: bool, turn: usize) -> String {
    let batch = A1Batch {
        anchors: vec![A1Anchor {
            reference: "a1".into(),
            kind: kind.into(),
            canonical: canonical.into(),
            aliases: aliases.iter().map(|a| (*a).to_string()).collect(),
            reality_status: "real".into(),
            evidence: vec![Provenance { turn_uid: uid(turn), anchor_role: AnchorRole::Human }],
            stable,
        }],
        mentions: Vec::new(),
        issues: Vec::new(),
        unresolved: Vec::new(),
    };
    let out = write_a1_with_owner(m, &batch, &ctx(), &owner(), &scope(), "run-redirect", NOW).unwrap();
    out.by_ref["a1"].id.clone()
}

/// 直接落一格锚，绕过生产写入口：跨 kind 的同名对在写侧就会被合流（W15b1），
/// 而本文件要测的正是"库里已经存在这样一对"时 pair 分类怎么读——那一对只能预置。
fn seeded(m: &MemoryHandle, kind: &str, canonical: &str, status: &str) -> String {
    let id = rc_memory::compute_mem_id(MemTable::Anchor, kind, canonical, "", &scope());
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind, scope_key,
                                status, profile_text, created_at_ms, updated_at_ms, norm_version)
             VALUES(?1,?2,?3,'real',NULL,?4,?5,?6,NULL,?7,?7,?8)",
            rusqlite::params![
                id,
                kind,
                canonical,
                scope().kind.as_str(),
                scope().key,
                status,
                NOW,
                rc_store::capability::NORM_VERSION as i64
            ],
        )
        .unwrap();
    id
}

fn fact_on(m: &MemoryHandle, subject: &str, text: &str, turn: usize) -> String {
    let _ = turn;
    put_fact_rows(m, &[FactWriteItem {
        subject_anchor_id: subject.into(),
        object_anchor_id: None,
        predicate: "likes".into(),
        value: Some("安静".into()),
        category: "observation".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: text.into(),
        evidence: vec![(uid(4), AnchorRole::Human)],
        text: text.into(),
        scope: scope(),
        source_ms: SRC_MS,
    }], &ctx(), &owner(), NOW).unwrap();
    rc_memory::compute_fact_id(subject, "likes", None, Some("安静"), "observation", "", text, &scope())
}

fn state_on(m: &MemoryHandle, subject: &str, value: &str, turn: usize) -> String {
    put_state_rows(m, &[StateWriteItem {
        subject_anchor_id: subject.into(),
        dimension: "mood".into(),
        value_after: value.into(),
        mode: "expressed".into(),
        text: value.to_string(),
        evidence: vec![(uid(turn), AnchorRole::Human)],
        scope: scope(),
        source_ms: SRC_MS,
    }], &ctx(), &owner(), NOW).unwrap();
    compute_state_id(subject, "mood", value, &uid(turn), &scope())
}

fn scalar(m: &MemoryHandle, sql: &str) -> i64 {
    m.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn status(m: &MemoryHandle, id: &str) -> String {
    m.conn().query_row("SELECT status FROM anchor WHERE id=?1", [id], |r| r.get(0)).unwrap()
}

/// 一条行级 mention（`memory_mention`；零搬运原则下合并**不许**动它）。
fn memory_mention_on(m: &MemoryHandle, mem_id: &str, anchor_id: &str) {
    m.conn().execute(
        "INSERT INTO memory_mention(mem_table, mem_id, anchor_id, source_agent, device_id,
                                     created_at_ms, norm_version)
         VALUES('card', ?1, ?2, 'agent-redirect', 'dev-redirect', ?3, 1)",
        rusqlite::params![mem_id, anchor_id, NOW],
    ).unwrap();
}

/// 1：写侧四动作一次到位——指向行、退场章、alias 来源三格、台账；旧锚不物理删。
#[test]
fn redirect_merge_writes_pointer_status_alias_and_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let to = anchor(m, "org", "幸存组织", &[], true, 1);
    let from = anchor(m, "org", "旧名组织", &[], false, 2);

    let report = merge_redirect(m, &from, &to, "同一组织", "user", NOW).unwrap();
    assert_eq!((report.from_id.as_str(), report.to_id.as_str()), (from.as_str(), to.as_str()));
    assert!(report.run_id.starts_with("redirect-"));
    assert_eq!(report.aliases_merged, 1);

    // 指向行一格不少。
    let row: (String, String, String, String) = m.conn().query_row(
        "SELECT from_id, to_id, run_id, reason FROM anchor_merge", [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
    assert_eq!((row.0.as_str(), row.1.as_str()), (from.as_str(), to.as_str()));
    assert_eq!((row.2.as_str(), row.3.as_str()), (report.run_id.as_str(), "同一组织"));

    // 退场章 + 不物理删；幸存锚照旧。
    assert_eq!(status(m, &from), "merged");
    assert_eq!(status(m, &to), "active");
    assert_eq!(scalar(m, "SELECT count(*) FROM anchor"), 2);

    // alias 来源三格：valid_from_ms/valid_to_ms/source_turn_uid 都按"程序动作"语义为 NULL
    //（幸存锚自己的名字照旧赢 display_name，不伪造日期）。
    let cells: (Option<i64>, Option<i64>, Option<String>) = m.conn().query_row(
        "SELECT valid_from_ms, valid_to_ms, source_turn_uid FROM alias
          WHERE anchor_id=?1 AND alias='旧名组织'",
        [&to], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
    assert_eq!(cells, (None, None, None));

    // 台账：MERGE 一行，after 指回 to。
    let after: String = m.conn().query_row(
        "SELECT after_json FROM memory_op WHERE op='MERGE' AND mem_table='anchor' AND mem_id=?1",
        [&from], |r| r.get(0)).unwrap();
    assert_eq!(after, format!("redirect:{to}"));

    // 解析：一跳到幸存锚。
    assert_eq!(resolve(m, &from).unwrap(),
               RedirectResolve::Merged { from: from.clone(), to: to.clone(), hops: 1 });
    assert_eq!(resolve(m, &to).unwrap(), RedirectResolve::Alive(to.clone()));
}

/// 2：零搬运——合并前后 mention.id / fact.subject_anchor_id / state_assertion.id 逐字节不变，
/// 三张表的行数一格不动。
#[test]
fn redirect_moves_nothing_ids_are_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let to = anchor(m, "org", "幸存组织", &[], true, 1);
    let from = anchor(m, "org", "旧名组织", &[], false, 2);
    let mention_id = format!("{}:{}", uid(3), from);
    m.conn().execute(
        "INSERT INTO mention(id, anchor_id, turn_uid, surface, span_start, span_end,
                             discourse_role, resolution, confidence, source_agent, device_id,
                             created_at_ms, norm_version)
         VALUES(?1,?2,?3,'旧名',NULL,NULL,'mentioned','resolved',0.9,'agent-redirect','dev-redirect',?4,1)",
        rusqlite::params![mention_id, from, uid(3), NOW]).unwrap();
    let fact_id = fact_on(m, &from, "旧名组织喜欢安静", 4);
    let state_id = state_on(m, &from, "平静", 5);
    let before: (String, String, String, i64, i64, i64) = m.conn().query_row(
        "SELECT (SELECT id FROM mention WHERE id=?1),
                (SELECT subject_anchor_id FROM fact WHERE id=?2),
                (SELECT subject_anchor_id FROM state_assertion WHERE id=?3),
                (SELECT count(*) FROM mention),
                (SELECT count(*) FROM fact),
                (SELECT count(*) FROM state_assertion)",
        rusqlite::params![mention_id, fact_id, state_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap();

    merge_redirect(m, &from, &to, "r", "user", NOW).unwrap();

    let after: (String, String, String, i64, i64, i64) = m.conn().query_row(
        "SELECT (SELECT id FROM mention WHERE id=?1),
                (SELECT subject_anchor_id FROM fact WHERE id=?2),
                (SELECT subject_anchor_id FROM state_assertion WHERE id=?3),
                (SELECT count(*) FROM mention),
                (SELECT count(*) FROM fact),
                (SELECT count(*) FROM state_assertion)",
        rusqlite::params![mention_id, fact_id, state_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap();
    assert_eq!(before, after, "零搬运：三格 id 与三表行数逐位相同");
}

/// 3：链式 A→B、B→C ⇒ A 解析到 C（to 已 merged 也允许写，链自然加长）。
#[test]
fn chained_redirects_resolve_to_the_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let c = anchor(m, "org", "终点组织", &[], true, 1);
    let b = anchor(m, "org", "中间组织", &[], false, 2);
    let a = anchor(m, "org", "起点组织", &[], false, 3);

    merge_redirect(m, &b, &c, "r1", "user", NOW).unwrap();
    assert_eq!(resolve(m, &b).unwrap(),
               RedirectResolve::Merged { from: b.clone(), to: c.clone(), hops: 1 });
    // to 已 merged 的二次指向：合法，链加长。
    merge_redirect(m, &a, &b, "r2", "user", NOW).unwrap();
    assert_eq!(resolve(m, &a).unwrap(),
               RedirectResolve::Merged { from: a.clone(), to: c.clone(), hops: 2 });
}

/// 4：防环（写侧+读侧）——from=to 拒；手塞自环/三环读侧都终止为 Broken:cycle，不栈溢出。
#[test]
fn cycles_terminate_as_broken_never_loop() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = anchor(m, "org", "甲", &[], true, 1);
    let b = anchor(m, "org", "乙", &[], true, 2);
    let c = anchor(m, "org", "丙", &[], true, 3);

    // 写侧：from=to。
    assert_eq!(merge_redirect(m, &a, &a, "r", "user", NOW), Err(RedirectError::SameAnchor));
    // 写侧：会成环的合并被拒（b→a 之后 a→b 会闭环）。先把 b 指向 a，再试 a→b。
    let _ = merge_redirect(m, &b, &a, "r", "user", NOW).unwrap();
    assert_eq!(merge_redirect(m, &a, &b, "r", "user", NOW), Err(RedirectError::Cycle));
    // 还原 b 的状态与指向行，造三环 A→B→C→A（直接塞行，模拟坏数据）。
    m.conn().execute("DELETE FROM anchor_merge WHERE from_id=?1", [&b]).unwrap();
    m.conn().execute("UPDATE anchor SET status='active' WHERE id=?1", [&b]).unwrap();
    for (f, t) in [(&a, &b), (&b, &c), (&c, &a)] {
        m.conn().execute(
            "INSERT INTO anchor_merge(from_id, to_id, run_id, reason, created_at_ms)
             VALUES(?1,?2,'bad','bad-data',?3)", rusqlite::params![f, t, NOW]).unwrap();
        m.conn().execute("UPDATE anchor SET status='merged' WHERE id=?1", [f]).unwrap();
    }
    // 三支都终止：Broken{cycle}，不栈溢出、不死循环。
    for id in [&a, &b, &c] {
        match resolve(m, id).unwrap() {
            RedirectResolve::Broken { reason, .. } => assert_eq!(reason, "cycle"),
            other => panic!("expected cycle, got {other:?}"),
        }
    }
    // 自环（单行 A→A）同样终止。
    m.conn().execute("DELETE FROM anchor_merge", []).unwrap();
    for id in [&a, &b, &c] {
        m.conn().execute("UPDATE anchor SET status='active' WHERE id=?1", [id]).unwrap();
    }
    m.conn().execute(
        "INSERT INTO anchor_merge(from_id, to_id, run_id, reason, created_at_ms)
         VALUES(?1,?1,'bad','self',?2)", rusqlite::params![a, NOW]).unwrap();
    m.conn().execute("UPDATE anchor SET status='merged' WHERE id=?1", [&a]).unwrap();
    match resolve(m, &a).unwrap() {
        RedirectResolve::Broken { reason, .. } => assert_eq!(reason, "cycle"),
        other => panic!("expected self-cycle, got {other:?}"),
    }
    // 读数面：断链清单点名。
    let broken = broken_redirects(m).unwrap();
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0].from_id, a);
    assert_eq!(broken[0].reason, "cycle");
}

/// 5：断链读数诚实——指向不存在的 to 是 Broken{missing_to}，不是 Missing；
/// merged 锚没有指向行也是 Broken{no_redirect}（可见，不静默成"没有"）。
#[test]
fn broken_chains_are_visible_not_silent() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let to = anchor(m, "org", "幸存组织", &[], true, 1);
    let from = anchor(m, "org", "旧名组织", &[], false, 2);
    let lonely = anchor(m, "org", "无指向的退场锚", &[], false, 3);
    merge_redirect(m, &from, &to, "r", "user", NOW).unwrap();
    // 物理删掉 to（CASCADE 清 alias；anchor_merge 无 FK，指向行留下）。
    m.conn().execute("DELETE FROM anchor WHERE id=?1", [&to]).unwrap();
    match resolve(m, &from).unwrap() {
        RedirectResolve::Broken { reason, stopped_at, .. } => {
            assert_eq!(reason, "missing_to");
            assert_eq!(stopped_at, to);
        }
        other => panic!("expected missing_to, got {other:?}"),
    }
    // merged 但没有指向行：Broken{no_redirect}。
    m.conn().execute("UPDATE anchor SET status='merged' WHERE id=?1", [&lonely]).unwrap();
    match resolve(m, &lonely).unwrap() {
        RedirectResolve::Broken { reason, .. } => assert_eq!(reason, "no_redirect"),
        other => panic!("expected no_redirect, got {other:?}"),
    }
    let broken = broken_redirects(m).unwrap();
    assert_eq!(broken.len(), 2, "{broken:?}");
    let mut reasons: Vec<&str> = broken.iter().map(|b| b.reason).collect();
    reasons.sort_unstable();
    assert_eq!(reasons, vec!["missing_to", "no_redirect"]);
}

/// 6：拒绝面闭集——from 已 merged（二次合并）、缺锚、只读句柄。
#[test]
fn redirects_are_refused_with_closed_set_reasons() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let to = anchor(m, "org", "幸存组织", &[], true, 1);
    let from = anchor(m, "org", "旧名组织", &[], false, 2);
    let alive = anchor(m, "org", "第三格锚", &[], true, 6);
    merge_redirect(m, &from, &to, "r", "user", NOW).unwrap();
    // 幂等兜底之下显式拒绝二次合并：from 已退场。
    assert_eq!(merge_redirect(m, &from, &to, "r", "user", NOW),
               Err(RedirectError::FromNotAlive));
    assert_eq!(scalar(m, "SELECT count(*) FROM anchor_merge"), 1, "不长第二行");
    // 缺锚两支（from 缺 / to 缺，from 侧用活锚避免踩 FromNotAlive）。
    assert_eq!(merge_redirect(m, "nonexistent-a", &to, "r", "user", NOW),
               Err(RedirectError::Missing));
    assert_eq!(merge_redirect(m, &alive, "nonexistent-b", "r", "user", NOW),
               Err(RedirectError::Missing));
    // 只读句柄。
    let path = dir.path().join("memory.db");
    let ro = rc_store::open_replica(&path).unwrap();
    assert_eq!(merge_redirect(&ro, &from, &to, "r", "user", NOW),
               Err(RedirectError::ReadOnly));
}

/// 7：回滚证明（库面）——删指向行 ⇒ 按 to 锚名字的行级 mention 命中消失；
/// 变异证明同型：删链上关键一跳，"旧锚 mention 仍可见"翻红。
#[test]
fn deleting_the_pointer_row_rolls_visibility_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let to = anchor(m, "org", "幸存组织", &[], true, 1);
    let from = anchor(m, "org", "旧名组织", &[], false, 2);
    // 一张卡，行级 mention 指向旧锚（旧锚的名字只在 from 的 canonical 上）。
    let card_id = compute_card_id("旧名组织提过的卡");
    m.conn().execute(
        "INSERT INTO card(id, kind, title, text, as_of_ms, verdict, source_agent, device_id,
                          scope_kind, scope_key, created_at_ms, norm_version)
         VALUES(?1,'note','旧名组织提过的卡','正文',?2,'unreviewed','agent-redirect','dev-redirect',
                'project',?3,?2,1)",
        rusqlite::params![card_id, SRC_MS, SCOPE]).unwrap();
    memory_mention_on(m, &card_id, &from);

    // 合并前：按 to 名字过滤命中不了旧锚的 mention。
    let key = MemKey { table: MemTable::Card, id: card_id.clone() };
    assert!(!rc_memory::reader::row_mentions_any(m, &key, std::slice::from_ref(&to)).unwrap());

    merge_redirect(m, &from, &to, "r", "user", NOW).unwrap();
    // 合并后：**仍可见**——to 锚的候选集经解析命中旧锚的 mention（读数 1 的库面形状）。
    assert!(rc_memory::reader::row_mentions_any(m, &key, std::slice::from_ref(&to)).unwrap(),
            "合并后旧锚的 mention 仍归属 to 锚可见");

    // 回滚：删指向行 ⇒ 回到合并前的读数（旧锚 mention 不再归属 to 锚；status 语义照旧）。
    m.conn().execute("DELETE FROM anchor_merge WHERE from_id=?1", [&from]).unwrap();
    assert!(!rc_memory::reader::row_mentions_any(m, &key, std::slice::from_ref(&to)).unwrap(),
            "删指向行后读数回到合并前");
}

/// 8：变异证明——A→B→C 链删掉 B→C 一跳 ⇒ 解析终点改变，"仍可见"的那条断言翻红。
/// （与用例 7 同一读者同一断言形状：链在 ⇒ 命中；关键一跳被删 ⇒ 不再命中。）
#[test]
fn mutation_deleting_a_chain_hop_turns_visibility_off() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let c = anchor(m, "org", "终点组织", &[], true, 1);
    let b = anchor(m, "org", "中间组织", &[], true, 2);
    let a = anchor(m, "org", "起点组织", &[], false, 3);
    let card_id = compute_card_id("起点组织提过的卡");
    m.conn().execute(
        "INSERT INTO card(id, kind, title, text, as_of_ms, verdict, source_agent, device_id,
                          scope_kind, scope_key, created_at_ms, norm_version)
         VALUES(?1,'note','起点组织提过的卡','正文',?2,'unreviewed','agent-redirect','dev-redirect',
                'project',?3,?2,1)",
        rusqlite::params![card_id, SRC_MS, SCOPE]).unwrap();
    memory_mention_on(m, &card_id, &a);
    let key = MemKey { table: MemTable::Card, id: card_id.clone() };

    merge_redirect(m, &b, &c, "r1", "user", NOW).unwrap();
    merge_redirect(m, &a, &b, "r2", "user", NOW).unwrap();
    assert!(rc_memory::reader::row_mentions_any(m, &key, std::slice::from_ref(&c)).unwrap(),
            "链完整：A 的 mention 按 C 的名字仍可见");

    // 变异：删掉关键一跳（B→C）⇒ 链断在 B（B merged 且无指向行 ⇒ Broken:no_redirect），
    // A 的渲染按 Broken 回落旧锚自身形状，按 C 名字的可见性翻红。
    m.conn().execute("DELETE FROM anchor_merge WHERE from_id=?1", [&b]).unwrap();
    assert_eq!(resolve(m, &a).unwrap(),
               RedirectResolve::Broken { from: a.clone(), stopped_at: b.clone(),
                                         reason: "no_redirect" });
    assert!(!rc_memory::reader::row_mentions_any(m, &key, std::slice::from_ref(&c)).unwrap(),
            "变异后恰好那条绿断言变红");
}

/// 9：pair 分类（复用 merge.rs 的思想，不 import）——A 同 kind 同 scope 名字相连；
/// 跨 kind B；scope 不同 C；名字不相连 C。
#[test]
fn pair_classification_reuses_the_discovery_idea() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let a = anchor(m, "org", "同名组织", &[], true, 1);
    let b = anchor(m, "org", "同名组织", &[], false, 2);
    let work = seeded(m, "work", "同名组织", "active");
    let elsewhere = anchor(m, "org", "跨库同名", &[], true, 4);
    m.conn().execute(
        "UPDATE anchor SET scope_key='other-proj' WHERE id=?1", [&elsewhere]).unwrap();
    let unlinked_to = anchor(m, "org", "另一个名字", &[], true, 5);

    assert_eq!(classify_pair(m, &b, &a).unwrap(), ("A", "same_kind_name_match"));
    assert_eq!(classify_pair(m, &b, &work).unwrap(), ("B", "cross_kind"));
    assert_eq!(classify_pair(m, &b, &elsewhere).unwrap(), ("C", "scope_mismatch"));
    assert_eq!(classify_pair(m, &b, &unlinked_to).unwrap(), ("C", "no_name_link"));
    assert_eq!(classify_pair(m, &b, "missing").unwrap(), ("missing", "missing_anchor"));
}
