// crates/rc-memory/tests/owner.rs
//! S1b-C2b：资源归属的唯一实现点 `rc_memory::owner`。
//!
//! 这里钉三件事：
//! ① 可见性的天然上界（行 scope 对 `visibility` 的封顶）与"取更保守一档"的全格；
//! ② 载荷 `scope_kind`（契约五值：session/agent/org/global）→ 记忆行词表
//!    （person|project|entity）的归一——**不放宽** `resource_owner` 的 CHECK；
//! ③ `insert` 的幂等语义（先写者赢，不覆盖）与 `existing_owner` 的读回，
//!    以及 `retention` 在写入点的真值归一（`days:x` 不许进表）。
use rc_memory::keys::{Scope, ScopeKind};
use rc_memory::owner::{self, OwnerRow, Visibility};
use rc_store::Root;

const VISIBILITIES: [Visibility; 5] = [
    Visibility::PrivateAgent,
    Visibility::Person,
    Visibility::Project,
    Visibility::Org,
    Visibility::Public,
];
const KINDS: [ScopeKind; 3] = [ScopeKind::Person, ScopeKind::Project, ScopeKind::Entity];

fn open() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    (dir, root)
}

fn owner_row(mem_id: &str) -> OwnerRow {
    OwnerRow {
        mem_table: "card".into(),
        mem_id: mem_id.into(),
        owner_principal_id: Some("p-1".into()),
        owner_agent_id: "agent-a".into(),
        created_by_agent: "agent-a".into(),
        visibility: Visibility::PrivateAgent,
        retention: "default".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-x".into() },
        source_agent: Some("agent-a".into()),
        device_id: Some("dev-a".into()),
        created_at_ms: 1000,
    }
}

/// 行 scope 的天然上界（设计 ownership-persistence §3.2）：
/// 人是人的事、项目是项目的事、实体不是项目（最多到 person）。
#[test]
fn ceiling_is_the_natural_upper_bound_of_the_row_scope() {
    assert_eq!(owner::ceiling(ScopeKind::Person), Visibility::Person);
    assert_eq!(owner::ceiling(ScopeKind::Project), Visibility::Project);
    assert_eq!(owner::ceiling(ScopeKind::Entity), Visibility::Person);
}

/// 5 visibility × 3 scope 全格：`effective` = min(session, ceiling(kind))，
/// 序 private_agent < person < project < org < public。逐格写死，任何一格挪动都红。
#[test]
fn effective_picks_the_more_conservative_of_session_visibility_and_scope_ceiling() {
    let expected = [
        (Visibility::PrivateAgent, ScopeKind::Person, Visibility::PrivateAgent),
        (Visibility::PrivateAgent, ScopeKind::Project, Visibility::PrivateAgent),
        (Visibility::PrivateAgent, ScopeKind::Entity, Visibility::PrivateAgent),
        (Visibility::Person, ScopeKind::Person, Visibility::Person),
        (Visibility::Person, ScopeKind::Project, Visibility::Person),
        (Visibility::Person, ScopeKind::Entity, Visibility::Person),
        (Visibility::Project, ScopeKind::Person, Visibility::Person),
        (Visibility::Project, ScopeKind::Project, Visibility::Project),
        (Visibility::Project, ScopeKind::Entity, Visibility::Person),
        (Visibility::Org, ScopeKind::Person, Visibility::Person),
        (Visibility::Org, ScopeKind::Project, Visibility::Project),
        (Visibility::Org, ScopeKind::Entity, Visibility::Person),
        (Visibility::Public, ScopeKind::Person, Visibility::Person),
        (Visibility::Public, ScopeKind::Project, Visibility::Project),
        (Visibility::Public, ScopeKind::Entity, Visibility::Person),
    ];
    // 全格覆盖：5 × 3 = 15，一行不漏。
    let mut covered = 0usize;
    for &(session, kind, want) in &expected {
        assert_eq!(
            owner::effective(session, kind),
            want,
            "effective({session:?}, {kind:?}) 不是更保守的一档"
        );
        covered += 1;
    }
    assert_eq!(covered, VISIBILITIES.len() * KINDS.len());
    // 单调性：session 的可见性一档档放宽时，结果不许变小（min 的性质）。
    for &kind in &KINDS {
        let mut prev = 0usize;
        for &v in &VISIBILITIES {
            let got = owner::effective(v, kind);
            let rank = VISIBILITIES.iter().position(|x| *x == got).unwrap();
            assert!(rank >= prev, "effective 不是单调的：{v:?} × {kind:?} 落到了 {got:?}");
            prev = rank;
        }
    }
}

/// 陷阱：载荷的 `scope_kind` 是契约五值（`ingest_v3::ScopeKind`：session/agent/org/global），
/// 而 `resource_owner.scope_kind` 只收记忆行词表（person|project|entity）。
/// 归一只认 person/entity，其余（含 project、空串、垃圾）一律 project。
#[test]
fn normalize_scope_kind_maps_the_contract_five_values_into_the_row_vocabulary() {
    assert_eq!(owner::normalize_scope_kind("person"), ScopeKind::Person);
    assert_eq!(owner::normalize_scope_kind("entity"), ScopeKind::Entity);
    assert_eq!(owner::normalize_scope_kind("project"), ScopeKind::Project);
    for raw in ["session", "agent", "org", "global", "", "Person", "entity ", "garbage"] {
        assert_eq!(
            owner::normalize_scope_kind(raw),
            ScopeKind::Project,
            "载荷 scope_kind={raw:?} 必须归一到 project（否则会撞 resource_owner 的 CHECK）"
        );
    }
}

/// `insert` 是 `INSERT OR IGNORE`：第一次 true、第二次 false 且**不改已有行**。
/// 这一格是"owner 占位先写者赢"的全部实现（设计 §9-7），不是一句注释。
#[test]
fn insert_is_idempotent_and_never_overwrites_the_first_owner() {
    let (_dir, root) = open();
    let tx = root.memory().conn().unchecked_transaction().unwrap();

    let first = owner_row("c-1");
    assert!(owner::insert(&tx, &first).unwrap(), "第一次 insert 必须是新写");

    // 第二行换掉每一个可变列：真正的差异只有 (mem_table, mem_id) 不变。
    let mut second = owner_row("c-1");
    second.owner_principal_id = None;
    second.owner_agent_id = "agent-b".into();
    second.created_by_agent = "agent-b".into();
    second.visibility = Visibility::Public;
    second.retention = "keep".into();
    second.scope = Scope { kind: ScopeKind::Entity, key: "proj-y".into() };
    second.created_at_ms = 2000;
    assert!(!owner::insert(&tx, &second).unwrap(), "第二次 insert 必须报告已有行");

    assert_eq!(
        owner::existing_owner(&tx, "card", "c-1").unwrap(),
        Some((Some("p-1".to_string()), "agent-a".to_string())),
        "先写者赢：已有的 owner 被覆盖了"
    );
    // 逐列读回：第二次的值一个都不许落库，且落的是 snake_case 字符串。
    let row: (String, String, String, String, i64) = tx
        .query_row(
            "SELECT visibility, retention, scope_kind, scope_key, created_at_ms
             FROM resource_owner WHERE mem_table='card' AND mem_id='c-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        (
            "private_agent".to_string(),
            "default".to_string(),
            "project".to_string(),
            "proj-x".to_string(),
            1000
        ),
        "落库值被第二次 insert 改了，或者 visibility/scope_kind 写成了 Rust 枚举名"
    );

    // 没有行 ⇒ None（不是空串、不是 panic）。
    assert_eq!(owner::existing_owner(&tx, "card", "nope").unwrap(), None);
    // 同 id 不同表是另一行（键是二元组）。
    let mut other_table = owner_row("c-1");
    other_table.mem_table = "anchor".into();
    other_table.owner_agent_id = "agent-b".into();
    other_table.created_by_agent = "agent-b".into();
    assert!(owner::insert(&tx, &other_table).unwrap());
    tx.commit().unwrap();
}

/// `retention` 在写入点就归一：只收 `default`/`keep`/`days:<正整数>`。
/// `days:x` 这种"CHECK 收得下、契约不认"的值不许进表（设计 §9-9 的真值在写入面）。
#[test]
fn insert_normalizes_retention_to_the_three_legal_shapes() {
    let (_dir, root) = open();
    let tx = root.memory().conn().unchecked_transaction().unwrap();
    let cases = [
        ("default", "default"),
        ("keep", "keep"),
        ("days:7", "days:7"),
        ("days:1", "days:1"),
        // 以下都是非法值：归一成 default，一个都不许按原样落库。
        ("days:x", "default"),
        ("days:0", "default"),
        ("days:-1", "default"),
        ("days:", "default"),
        ("sometimes", "default"),
        ("", "default"),
        ("DAYS:7", "default"),
    ];
    for (i, (raw, want)) in cases.iter().enumerate() {
        let mut row = owner_row(&format!("c-ret-{i}"));
        row.retention = (*raw).to_string();
        assert!(owner::insert(&tx, &row).unwrap(), "retention={raw:?} 没写进去");
        let got: String = tx
            .query_row(
                "SELECT retention FROM resource_owner WHERE mem_id=?1",
                [format!("c-ret-{i}")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(&got, want, "retention={raw:?} 落库成了 {got:?}");
    }
    tx.commit().unwrap();
}

/// `sessions.visibility` 的字符串 → 枚举（C3 把 `hub::rows` 的私有副本搬到这里，
/// 成为唯一实现点）。认不出的一律 `private_agent`：读侧最保守的一档，
/// 与 `sessions` 的 NULL 语义（COALESCE 成 `private_agent`）同向。
#[test]
fn parse_visibility_accepts_the_five_literals_and_falls_back_to_private_agent() {
    assert_eq!(owner::parse_visibility("private_agent"), Visibility::PrivateAgent);
    assert_eq!(owner::parse_visibility("person"), Visibility::Person);
    assert_eq!(owner::parse_visibility("project"), Visibility::Project);
    assert_eq!(owner::parse_visibility("org"), Visibility::Org);
    assert_eq!(owner::parse_visibility("public"), Visibility::Public);
    for raw in ["", "Person", "PROJECT", " private_agent", "visible_to_friends", "garbage", "0"] {
        assert_eq!(
            owner::parse_visibility(raw),
            Visibility::PrivateAgent,
            "visibility={raw:?} 认不出时必须落到 private_agent（最保守一档）"
        );
    }
}

/// `sessions.retention` 的写入点归一（C3 的 `OwnerCtx.retention` 用它）：
/// 只收 `default`/`keep`/`days:<正整数>`，其余（含 `days:0` 与 `days:x`）都归 `default`。
/// `insert` 返回的落库值与这个函数逐格一致——同一个实现点，不是两把尺子。
#[test]
fn normalize_retention_keeps_the_three_legal_shapes_and_defaults_everything_else() {
    let cases = [
        ("default", "default"),
        ("keep", "keep"),
        ("days:7", "days:7"),
        ("days:1", "days:1"),
        ("days:3650", "days:3650"),
        ("days:0", "default"),
        ("days:x", "default"),
        ("days:-1", "default"),
        ("days:", "default"),
        ("sometimes", "default"),
        ("", "default"),
        ("DAYS:7", "default"),
        (" keep", "default"),
    ];
    for (raw, want) in cases {
        assert_eq!(
            owner::normalize_retention(raw),
            want,
            "retention={raw:?} 的归一是 {want:?}"
        );
    }
}

/// `visibility` 落库必须是 serde snake_case（`private_agent` 而不是 `PrivateAgent`）。
/// 五格逐个写一遍：大小写/下划线写错时 CHECK 会拒、或读侧认不出。
#[test]
fn visibility_is_stored_as_the_snake_case_slug_of_the_contract() {
    let (_dir, root) = open();
    let tx = root.memory().conn().unchecked_transaction().unwrap();
    for (i, &v) in VISIBILITIES.iter().enumerate() {
        let mut row = owner_row(&format!("c-vis-{i}"));
        row.visibility = v;
        // 上界（`effective`）是调用方算出来的，不是表约束：表只管词表。
        // 这里直接写五格原文，钉的是"Rust 枚举 → 契约字面量"这一跳。
        assert!(owner::insert(&tx, &row).unwrap());
        let got: String = tx
            .query_row(
                "SELECT visibility FROM resource_owner WHERE mem_id=?1",
                [format!("c-vis-{i}")],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            got,
            ["private_agent", "person", "project", "org", "public"][i],
            "visibility={v:?} 落库不是 snake_case 契约字面量"
        );
    }
    tx.commit().unwrap();
}
