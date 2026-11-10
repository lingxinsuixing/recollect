//! S3c-3a：`rc_memory::writers_rows::insert_fact_row` 的写库面（派发档 §2）。
//!
//! 夹具走真库：`Root::open(tempdir)` + 一条先铺好的 `anchor`（fact 的外键目标）。
//! 每条用例只制造一个违规，断言只看错误族与库里的行：
//!
//! 1. 成功行：一次插入 1 行，归属逐列正确（`created_by_agent` 取批、`visibility` 过
//!    `effective` 天花板、`retention` 走归一）；
//! 2. 重复 id：第二次返回 0，fact 行与既有归属都不被后到的调用者改写；
//! 3. 列校验：未知列 / 重复列 / 空列集 / 缺 id/text/evidence / 非 TEXT / 空串 ⇒
//!    `MemError::Sql("rows: invalid fact column …")`，且错误不回显列名；
//! 4. 写库前的 CHECK / FK 错误仍由 SQLite 返回 `MemError::Sql`，零行零归属。
//!
//! 正反例只用通用占位名（张三/某项目），不含任何真实语料专名或 holdout。
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_rows::insert_fact_row;
use rc_memory::{BatchCtx, MemError, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};
use rusqlite::types::Value;

const NOW: i64 = 1_700_000_999_000;
const SRC_MS: i64 = 1_700_000_000_000;
const ANCHOR: &str = "anchor-zhang";
const DEVICE: &str = "dev-rows";

fn ctx() -> BatchCtx {
    // 与 `BatchCtx::synthetic()` 不同的一对值：P 独占列若偷偷用合成常量，下面会红。
    BatchCtx {
        source_agent: "agent-rows".into(),
        device_id: DEVICE.into(),
    }
}

fn scope() -> Scope {
    Scope {
        kind: ScopeKind::Project,
        key: "proj-rows".into(),
    }
}

/// owner 的 visibility 比行 scope 宽：落库必须是 `min(session, ceiling(project)) = project`。
fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-rows".into()),
        owner_agent_id: "agent-owner".into(),
        visibility: Visibility::Public,
        retention: "keep".into(),
    }
}

fn text(value: &str) -> Value {
    Value::Text(value.to_string())
}

fn col(name: &str, value: Value) -> (String, Value) {
    (name.to_string(), value)
}

/// 一行最小合法 fact：三格身份 + 全部 NOT NULL 列。
fn row(id: &str) -> Vec<(String, Value)> {
    vec![
        col("id", text(id)),
        col("subject_anchor_id", text(ANCHOR)),
        col("predicate", text("decide")),
        col("category", text("decision")),
        col("mode", text("stated")),
        col("text", text("发布推迟到 12 月 3 日")),
        col("evidence_turn_uid", text(&"a".repeat(64))),
        col("scope_kind", text("project")),
        col("scope_key", text("proj-rows")),
        col("source_agent", text("agent-rows")),
        col("device_id", text(DEVICE)),
        col("created_at_ms", Value::Integer(SRC_MS)),
        col("norm_version", Value::Integer(1)),
    ]
}

fn set(values: &mut [(String, Value)], name: &str, value: Value) {
    let slot = values
        .iter_mut()
        .find(|(column, _)| column == name)
        .expect("夹具必须真有这一列");
    slot.1 = value;
}

fn remove(values: &mut Vec<(String, Value)>, name: &str) {
    let index = values
        .iter()
        .position(|(column, _)| column == name)
        .expect("夹具必须真有这一列");
    values.remove(index);
}

fn count(m: &MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

/// `resource_owner` 的逐列读数（只在测试里拼列名，列名是常量）。
fn owner_col<T: rusqlite::types::FromSql>(m: &MemoryHandle, column: &str, id: &str) -> T {
    m.conn()
        .query_row(
            &format!(
                "SELECT {column} FROM resource_owner WHERE mem_table='fact' AND mem_id=?1"
            ),
            [id],
            |row| row.get(0),
        )
        .unwrap()
}

fn fact_text(m: &MemoryHandle, id: &str) -> String {
    m.conn()
        .query_row("SELECT text FROM fact WHERE id=?1", [id], |r| r.get(0))
        .unwrap()
}

fn seed_anchor(m: &MemoryHandle) {
    m.conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1,'person','张三','unknown',NULL,'project','proj-rows','active',NULL,?2,?2,?3)",
            rusqlite::params![ANCHOR, SRC_MS, rc_store::capability::NORM_VERSION as i64],
        )
        .unwrap();
}

fn expect_invalid(result: Result<usize, MemError>) {
    match result {
        Err(MemError::Sql(message)) => {
            assert!(
                message.starts_with("rows: invalid fact column"),
                "列校验必须走同一个静态错误族，得到：{message}"
            );
        }
        Err(other) => panic!("期望 MemError::Sql，得到 {other:?}"),
        Ok(n) => panic!("期望列校验失败，却插入 {n} 行"),
    }
}

// ── 1. 成功行 + 归属 ─────────────────────────────────────────────────────────

#[test]
fn insert_writes_the_row_and_its_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory());

    let n = insert_fact_row(root.memory(), &row("fact-1"), &owner(), &scope(), &ctx(), NOW)
        .unwrap();
    assert_eq!(n, 1, "首写必须真是新行");
    assert_eq!(count(root.memory(), "fact"), 1);
    assert_eq!(count(root.memory(), "resource_owner"), 1);

    // 行值原样落库（没有被本层"顺手"改写）。
    assert_eq!(fact_text(root.memory(), "fact-1"), "发布推迟到 12 月 3 日");

    // 归属：表名/身份/写者/上界/归一/范围/来源全在。
    assert_eq!(owner_col::<String>(root.memory(), "mem_table", "fact-1"), "fact");
    assert_eq!(
        owner_col::<Option<String>>(root.memory(), "owner_principal_id", "fact-1"),
        Some("p-rows".to_string())
    );
    assert_eq!(
        owner_col::<String>(root.memory(), "owner_agent_id", "fact-1"),
        "agent-owner"
    );
    assert_eq!(
        owner_col::<String>(root.memory(), "created_by_agent", "fact-1"),
        "agent-rows"
    );
    // `effective(Public, Project) = Project`：会话声明只是上界，行 scope 再压一次。
    assert_eq!(
        owner_col::<String>(root.memory(), "visibility", "fact-1"),
        "project"
    );
    assert_eq!(owner_col::<String>(root.memory(), "retention", "fact-1"), "keep");
    assert_eq!(owner_col::<String>(root.memory(), "scope_kind", "fact-1"), "project");
    assert_eq!(owner_col::<String>(root.memory(), "scope_key", "fact-1"), "proj-rows");
    assert_eq!(
        owner_col::<Option<String>>(root.memory(), "source_agent", "fact-1"),
        Some("agent-rows".to_string())
    );
    assert_eq!(
        owner_col::<Option<String>>(root.memory(), "device_id", "fact-1"),
        Some(DEVICE.to_string())
    );
    assert_eq!(owner_col::<i64>(root.memory(), "created_at_ms", "fact-1"), NOW);
}

// ── 2. 重复 id：0 行、既有行与归属不变 ──────────────────────────────────────

#[test]
fn duplicate_id_is_idempotent_and_keeps_the_first_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory());

    let first = insert_fact_row(root.memory(), &row("fact-1"), &owner(), &scope(), &ctx(), NOW)
        .unwrap();
    assert_eq!(first, 1);

    let mut second_row = row("fact-1");
    set(&mut second_row, "text", text("后到的另一段文字"));
    let other_owner = OwnerCtx {
        owner_principal_id: Some("p-other".into()),
        owner_agent_id: "agent-other".into(),
        visibility: Visibility::PrivateAgent,
        retention: "default".into(),
    };
    let second = insert_fact_row(
        root.memory(),
        &second_row,
        &other_owner,
        &scope(),
        &ctx(),
        NOW + 1_000,
    )
    .unwrap();
    assert_eq!(second, 0, "同 id 重放必须命中幂等，不新增行");
    assert_eq!(count(root.memory(), "fact"), 1);
    assert_eq!(count(root.memory(), "resource_owner"), 1);
    assert_eq!(
        fact_text(root.memory(), "fact-1"),
        "发布推迟到 12 月 3 日",
        "ON CONFLICT DO NOTHING 不许改写既有行"
    );
    assert_eq!(
        owner_col::<String>(root.memory(), "owner_agent_id", "fact-1"),
        "agent-owner",
        "归属是先写者赢，后到的调用者不许顶替"
    );
    assert_eq!(owner_col::<i64>(root.memory(), "created_at_ms", "fact-1"), NOW);
}

// ── 3. 列校验：静态错误族，不回显列名 ───────────────────────────────────────

#[test]
fn unknown_column_is_rejected_without_echoing_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory());

    let mut values = row("fact-1");
    values.push(col("not_a_fact_column", text("x")));
    match insert_fact_row(root.memory(), &values, &owner(), &scope(), &ctx(), NOW) {
        Err(MemError::Sql(message)) => {
            assert!(message.starts_with("rows: invalid fact column"));
            assert!(
                !message.contains("not_a_fact_column"),
                "守卫 12：错误不许回显列名，得到：{message}"
            );
        }
        other => panic!("期望列校验失败，得到 {other:?}"),
    }
    assert_eq!(count(root.memory(), "fact"), 0);
}

#[test]
fn duplicate_column_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory());

    let mut values = row("fact-1");
    values.push(col("text", text("第二遍")));
    expect_invalid(insert_fact_row(
        root.memory(),
        &values,
        &owner(),
        &scope(),
        &ctx(),
        NOW,
    ));
    assert_eq!(count(root.memory(), "fact"), 0);
}

#[test]
fn empty_column_set_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory());

    expect_invalid(insert_fact_row(root.memory(), &[], &owner(), &scope(), &ctx(), NOW));
    assert_eq!(count(root.memory(), "fact"), 0);
}

#[test]
fn missing_required_columns_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory());

    for required in ["id", "text", "evidence_turn_uid"] {
        let mut values = row("fact-1");
        remove(&mut values, required);
        expect_invalid(insert_fact_row(
            root.memory(),
            &values,
            &owner(),
            &scope(),
            &ctx(),
            NOW,
        ));
    }
    assert_eq!(count(root.memory(), "fact"), 0);
}

#[test]
fn required_columns_must_be_non_empty_text() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory());

    // 非 TEXT。
    let mut non_text = row("fact-1");
    set(&mut non_text, "id", Value::Integer(7));
    expect_invalid(insert_fact_row(
        root.memory(),
        &non_text,
        &owner(),
        &scope(),
        &ctx(),
        NOW,
    ));

    // 空串（text）。
    let mut empty_text = row("fact-1");
    set(&mut empty_text, "text", text(""));
    expect_invalid(insert_fact_row(
        root.memory(),
        &empty_text,
        &owner(),
        &scope(),
        &ctx(),
        NOW,
    ));

    // 空串（evidence_turn_uid）。
    let mut empty_evidence = row("fact-1");
    set(&mut empty_evidence, "evidence_turn_uid", text(""));
    expect_invalid(insert_fact_row(
        root.memory(),
        &empty_evidence,
        &owner(),
        &scope(),
        &ctx(),
        NOW,
    ));

    assert_eq!(count(root.memory(), "fact"), 0);
}

// ── 4. 写库前的 CHECK / FK 错误仍由 SQLite 返回 MemError::Sql ────────────────

#[test]
fn check_violation_is_surfaced_as_sql_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    seed_anchor(root.memory());

    let mut values = row("fact-1");
    set(&mut values, "category", text("bogus"));
    match insert_fact_row(root.memory(), &values, &owner(), &scope(), &ctx(), NOW) {
        Err(MemError::Sql(message)) => assert!(
            !message.starts_with("rows: invalid fact column"),
            "CHECK 错误不是列白名单错误：{message}"
        ),
        other => panic!("期望 SQLite CHECK 错误，得到 {other:?}"),
    }
    assert_eq!(count(root.memory(), "fact"), 0, "失败必须整事务回滚");
    assert_eq!(count(root.memory(), "resource_owner"), 0);
}

#[test]
fn foreign_key_violation_is_surfaced_as_sql_and_rolls_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();

    // 不铺锚：subject 的 FK 目标不存在。
    match insert_fact_row(root.memory(), &row("fact-1"), &owner(), &scope(), &ctx(), NOW) {
        Err(MemError::Sql(message)) => assert!(
            !message.starts_with("rows: invalid fact column"),
            "FK 错误不是列白名单错误：{message}"
        ),
        other => panic!("期望 SQLite FK 错误，得到 {other:?}"),
    }
    assert_eq!(count(root.memory(), "fact"), 0);
    assert_eq!(count(root.memory(), "resource_owner"), 0);
}

// 非 fact 场景不用单测：`insert_fact_row` 没有表参数，换表这件事在签名上就不可达。
