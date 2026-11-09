//! W22a（审计 P2-3 第一半）：活锚 id 的动态复算读数（`issues::anchor_id_audit`）。
//!
//! 四条用例各钉一支：空库 ⇒ `0/0`；id 由 `compute_mem_id` 算出的两条锚 ⇒ `2/0`；
//! 手插一行对不上的 id ⇒ `3/1` 且其余不受影响；挂未解决退场账的行不进 checked。
//! 最后一条是这一格的牙：**只查活锚**——退场那一支真去掉时它会立刻红（那一行的 id 也是错的，
//! 被数进来就是 mismatch 2）。
//!
//! 变异（报告里逐条记红行）：把重算时的归属键换成空串、或把正名换掉 ⇒ 用例 2 的 mismatch
//! 变 1。这两条变异打的是"复算真的按行上的列走"，而不是拿库里那串 hex 回读一遍。
use rc_memory::issues::anchor_id_audit;
use rc_memory::{compute_mem_id, MemTable, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;

fn fresh() -> (tempfile::TempDir, Root) {
    let d = tempfile::tempdir().unwrap();
    let r = Root::open(d.path()).unwrap();
    (d, r)
}

/// 一行锚：`id` 与其余列分开传，正是为了让"对得上"与"对不上"两种夹具共用一条插入。
fn insert_raw(
    m: &MemoryHandle,
    id: &str,
    kind: &str,
    canonical: &str,
    scope_kind: &str,
    scope_key: &str,
) {
    m.conn()
        .execute(
            "INSERT INTO anchor(id,kind,canonical,reality_status,scope_kind,scope_key,status,
                                created_at_ms,updated_at_ms,norm_version)
             VALUES(?1,?2,?3,'real',?4,?5,'active',?6,?6,1)",
            (id, kind, canonical, scope_kind, scope_key, NOW),
        )
        .unwrap();
}

/// 一行**id 就是自己 preimage** 的锚：id 交给生产算法算，其余列逐字是它的入参。
fn consistent_anchor(m: &MemoryHandle, kind: &str, canonical: &str, scope: &Scope) {
    let id = compute_mem_id(MemTable::Anchor, kind, canonical, "", scope);
    insert_raw(m, &id, kind, canonical, scope.kind.as_str(), &scope.key);
}

fn retire(m: &MemoryHandle, anchor_id: &str) {
    m.conn()
        .execute(
            "INSERT INTO retirement(mem_table,mem_id,reason,created_at_ms,resolved_at_ms)
             VALUES('anchor',?1,'test',?2,NULL)",
            (anchor_id, NOW),
        )
        .unwrap();
}

#[test]
fn an_empty_store_reads_zero_checked_and_zero_mismatch() {
    let (_d, root) = fresh();
    assert_eq!(
        anchor_id_audit(root.memory()).unwrap(),
        (0, 0),
        "空库这一格是 0/0 这个读数，不是读失败"
    );
}

#[test]
fn anchors_whose_id_is_their_own_preimage_have_no_mismatch() {
    let (_d, root) = fresh();
    let m = root.memory();
    let proj = Scope { kind: ScopeKind::Project, key: "proj-a".into() };
    let person = Scope { kind: ScopeKind::Person, key: "p-1".into() };
    consistent_anchor(m, "person", "张三", &proj);
    // 第二条换 kind 也换 scope：换任一段都会换 preimage，两条各自对得上才算数。
    consistent_anchor(m, "work", "部署手册", &person);
    assert_eq!(anchor_id_audit(m).unwrap(), (2, 0), "两条都对得上 ⇒ 2/0");
}

#[test]
fn one_anchor_with_a_foreign_id_is_one_mismatch_and_the_others_are_unaffected() {
    let (_d, root) = fresh();
    let m = root.memory();
    let proj = Scope { kind: ScopeKind::Project, key: "proj-a".into() };
    consistent_anchor(m, "person", "张三", &proj);
    let person = Scope { kind: ScopeKind::Person, key: "p-1".into() };
    consistent_anchor(m, "work", "部署手册", &person);
    // 一行 64 位 hex，但不是这三列的 preimage：存量脏行的形状。
    insert_raw(m, &"e".repeat(64), "person", "李四", "project", "proj-a");
    assert_eq!(
        anchor_id_audit(m).unwrap(),
        (3, 1),
        "三行都比过、其中一行对不上：其余两行不受影响"
    );
}

#[test]
fn an_anchor_on_an_open_retirement_is_not_checked() {
    let (_d, root) = fresh();
    let m = root.memory();
    let proj = Scope { kind: ScopeKind::Project, key: "proj-a".into() };
    consistent_anchor(m, "person", "张三", &proj);
    let person = Scope { kind: ScopeKind::Person, key: "p-1".into() };
    consistent_anchor(m, "work", "部署手册", &person);
    // 退场行故意用错的 id：数进来就会变成 (3,1)，这一支有没有牙由这两个数一起看。
    let gone = "f".repeat(64);
    insert_raw(m, &gone, "person", "王五", "project", "proj-a");
    retire(m, &gone);
    assert_eq!(
        anchor_id_audit(m).unwrap(),
        (2, 0),
        "挂着未解决退场账的锚已经被人处置过，不进 checked（夹具前提：它的 id 也是错的）"
    );
}
