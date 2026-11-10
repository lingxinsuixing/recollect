// crates/hub/tests/ownership_backfill_cli.rs
//! S1b-C4b2a：`hub ownership backfill` 的 CLI 面（设计 ownership-persistence §6 / §8-C4）。
//!
//! 判定面是**真进程的 stdout + 库里的行**：回填有没有接线、登记的行对不对、
//! 重跑是不是 0 新增，只有这两样看得见。
//!
//! 口径（与 `hub::ownership` 的实现同一句话）：
//! - 能从 `provenance.turn_uid → turns → sessions` 推出来的行，owner 取那一格会话身份；
//! - 推不出的行（无出处 / 跨 ≥2 个身份 / session 解析不到）落哨兵 ——
//!   `owner_agent_id = 行 source_agent，没有该列（anchor）才是字面量 "unknown"`，
//!   `visibility=private_agent`、`retention=default`，并写一条
//!   `memory_op(op='NOOP', reason='owner_backfill_unresolved', actor='program')`；
//! - 已有归属行一个都不覆盖，孤儿归属行（`(card|anchor, id)` 已不存在）删掉。
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch;
use rc_store::Root;
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hub binary")
}

fn data_arg(data: &Path) -> String {
    data.to_str().unwrap().to_string()
}

// ───────────────────────────── 夹具 ─────────────────────────────

fn session(key: &str, device: &str, agent: &str) -> SessionId {
    SessionId {
        session_key: key.into(),
        device_id: device.into(),
        source_agent: agent.into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    }
}

fn turn(uid: &str, body: &str, created_at_ms: i64) -> NormalizedTurn {
    NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: uid.into(),
        body: body.into(),
        body_fp: compute_body_fp(body),
        role: "human".into(),
        created_at_ms,
    }
}

/// 一个会话 + 若干轮次；返回 `compute_turn_uid` 给出的 uid（与 `bodies` 同序）。
/// 走生产唯一写入口 `insert_batch` ⇒ `sessions` 的 7 个归属列先落保守默认
/// （owner=source_agent / private / default），回调用例再按需改成操作员口径。
fn seed_session(
    root: &mut Root,
    key: &str,
    device: &str,
    agent: &str,
    bodies: &[(&str, i64)],
) -> Vec<String> {
    let uids: Vec<String> = bodies
        .iter()
        .map(|(body, created)| compute_turn_uid(device, key, "human", body, *created))
        .collect();
    let turns: Vec<NormalizedTurn> = bodies
        .iter()
        .zip(&uids)
        .map(|((body, created), uid)| turn(uid, body, *created))
        .collect();
    insert_batch(root.transcript_mut(), &session(key, device, agent), &turns, 10_000).unwrap();
    uids
}

/// 运维对会话归属的声明（`ownership_src='operator'` 那 6 格，逐项由用例控制）。
struct Declared<'a> {
    principal: Option<&'a str>,
    agent: &'a str,
    scope_key: &'a str,
    visibility: &'a str,
    retention: &'a str,
}

/// 把会话的归属改成"运维声明的身份"。
fn own_session(root: &Root, key: &str, device: &str, d: Declared<'_>) {
    let n = root
        .transcript()
        .conn()
        .execute(
            "UPDATE sessions
                SET owner_principal_id=?3, owner_agent_id=?4, scope_kind='project',
                    scope_key=?5, visibility=?6, retention=?7, ownership_src='operator'
              WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![key, device, d.principal, d.agent, d.scope_key, d.visibility, d.retention],
        )
        .unwrap();
    assert_eq!(n, 1, "会话没改成：{key}/{device}");
}

/// 直接把 7 个归属列抹成 NULL：模拟 v4 迁移留下的旧行（设计 §1.2）。
fn forget_session_ownership(root: &Root, key: &str, device: &str) {
    let n = root
        .transcript()
        .conn()
        .execute(
            "UPDATE sessions
                SET owner_principal_id=NULL, owner_agent_id=NULL, scope_kind=NULL,
                    scope_key=NULL, visibility=NULL, retention=NULL, ownership_src=NULL
              WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![key, device],
        )
        .unwrap();
    assert_eq!(n, 1, "会话没抹成 NULL：{key}/{device}");
}

fn card_row(root: &Root, id: &str, scope_kind: &str, scope_key: &str, agent: &str, device: &str, created_at_ms: i64) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO card(id, kind, title, text, scope_kind, scope_key, as_of_ms,
                              source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1, 'fact', ?1, '合成卡正文', ?2, ?3, ?4, ?5, ?6, ?4, 1)",
            rusqlite::params![id, scope_kind, scope_key, created_at_ms, agent, device],
        )
        .unwrap();
}

/// anchor 没有 `source_agent`/`device_id` 列（设计 §2.1）⇒ 归属行这两格必须是 NULL。
fn anchor_row(root: &Root, id: &str, scope_kind: &str, scope_key: &str, created_at_ms: i64) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, scope_kind, scope_key,
                                created_at_ms, updated_at_ms, norm_version)
             VALUES(?1, 'concept', ?1, ?2, ?3, ?4, ?4, 1)",
            rusqlite::params![id, scope_kind, scope_key, created_at_ms],
        )
        .unwrap();
}

fn prov(root: &Root, mem_table: &str, mem_id: &str, turn_uid: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
             VALUES(?1, ?2, ?3, 'human')",
            rusqlite::params![mem_table, mem_id, turn_uid],
        )
        .unwrap();
}

/// 预插一行归属（孤儿用例的靶），列值与 `resource_owner` 的 NOT NULL 对齐。
fn synthetic_owner(root: &Root, mem_table: &str, mem_id: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES(?1, ?2, NULL, 'ghost-agent', 'ghost-agent', 'private_agent', 'default',
                    'project', 'ghost-scope', NULL, NULL, 1)",
            rusqlite::params![mem_table, mem_id],
        )
        .unwrap();
}

// ───────────────────────────── 读数 ─────────────────────────────

#[derive(Debug, PartialEq, Eq)]
struct Owner {
    principal: Option<String>,
    agent: String,
    created_by: String,
    visibility: String,
    retention: String,
    scope_kind: String,
    scope_key: String,
    source_agent: Option<String>,
    device_id: Option<String>,
    created_at_ms: i64,
}

fn owner_of(data: &Path, mem_table: &str, mem_id: &str) -> Option<Owner> {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, created_by_agent, visibility, retention,
                    scope_kind, scope_key, source_agent, device_id, created_at_ms
               FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
            [mem_table, mem_id],
            |r| {
                Ok(Owner {
                    principal: r.get(0)?,
                    agent: r.get(1)?,
                    created_by: r.get(2)?,
                    visibility: r.get(3)?,
                    retention: r.get(4)?,
                    scope_kind: r.get(5)?,
                    scope_key: r.get(6)?,
                    source_agent: r.get(7)?,
                    device_id: r.get(8)?,
                    created_at_ms: r.get(9)?,
                })
            },
        )
        .optional()
        .unwrap()
}

fn owner_rows_digest(data: &Path) -> Vec<String> {
    let root = Root::open(data).unwrap();
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT mem_table, mem_id, IFNULL(owner_principal_id,'<null>'), owner_agent_id,
                    created_by_agent, visibility, retention, scope_kind, scope_key,
                    IFNULL(source_agent,'<null>'), IFNULL(device_id,'<null>'), created_at_ms
               FROM resource_owner ORDER BY mem_table, mem_id",
        )
        .unwrap();
    let rows = st
        .query_map([], |r| {
            Ok(format!(
                "{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}|{}",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
                r.get::<_, i64>(11)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

fn memory_op_count(data: &Path, mem_table: &str, mem_id: &str, reason: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row(
            "SELECT count(*) FROM memory_op
              WHERE op='NOOP' AND mem_table=?1 AND mem_id=?2 AND reason=?3 AND actor='program'",
            rusqlite::params![mem_table, mem_id, reason],
            |r| r.get(0),
        )
        .unwrap()
}

fn unresolved_counter(data: &Path) -> i64 {
    let root = Root::open(data).unwrap();
    root.ledger()
        .conn()
        .query_row(
            "SELECT count FROM usage_counter
              WHERE subject_kind='maintenance' AND subject_id='ownership'
                AND metric='ownership_unresolved'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
        .unwrap_or(0)
}

/// 跑一次 `ownership backfill --json`，失败时把 stderr 带进断言消息。
fn backfill(data: &Path) -> Value {
    let dd = data_arg(data);
    let out = run(&["ownership", "backfill", "--data-dir", &dd, "--json"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "回填必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}

// ───────────────────────────── 用例 ─────────────────────────────

/// ① 正常：一个会话（运维声明 p1/alice、project、keep）+ card 与 anchor 各一条 provenance
/// ⇒ 两行都写、0 未决；`created_by` 取行自己的 source_agent（card）/
/// 第一个会话的 source_agent（anchor 没有该列）；scope 取**行**的列而不是会话的。
#[test]
fn a_resolvable_card_and_anchor_get_the_session_owner() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let uids = seed_session(&mut root, "s-a", "dev-a", "qoder", &[("卡正文", 100), ("锚正文", 200)]);
        own_session(
            &root,
            "s-a",
            "dev-a",
            Declared { principal: Some("p1"), agent: "alice", scope_key: "proj-session", visibility: "project", retention: "keep" },
        );
        card_row(&root, "card-1", "project", "proj-row", "writer", "dev-mem", 900);
        prov(&root, "card", "card-1", &uids[0]);
        anchor_row(&root, "anchor-1", "project", "proj-row", 800);
        prov(&root, "anchor", "anchor-1", &uids[1]);
    }
    let report = backfill(&data);
    assert_eq!(report["rows_scanned"], 2);
    assert_eq!(report["already_owned"], 0);
    assert_eq!(report["owners_written"], 2);
    assert_eq!(report["unresolved"], 0);
    assert_eq!(report["orphans_removed"], 0);

    assert_eq!(
        owner_of(&data, "card", "card-1"),
        Some(Owner {
            principal: Some("p1".into()),
            agent: "alice".into(),
            // 行有 source_agent ⇒ created_by 取行的产出者，不取会话的。
            created_by: "writer".into(),
            // effective(project, ceiling(行 project)) = project
            visibility: "project".into(),
            retention: "keep".into(),
            scope_kind: "project".into(),
            // 行自己的 scope_key 优先，不是会话的 'proj-session'。
            scope_key: "proj-row".into(),
            source_agent: Some("writer".into()),
            device_id: Some("dev-mem".into()),
            created_at_ms: 900,
        })
    );
    assert_eq!(
        owner_of(&data, "anchor", "anchor-1"),
        Some(Owner {
            principal: Some("p1".into()),
            agent: "alice".into(),
            // anchor 没有 source_agent 列 ⇒ created_by 退回会话的 source_agent。
            created_by: "qoder".into(),
            visibility: "project".into(),
            retention: "keep".into(),
            scope_kind: "project".into(),
            scope_key: "proj-row".into(),
            source_agent: None,
            device_id: None,
            created_at_ms: 800,
        })
    );
}

/// ② 未决：一行的 provenance 跨两个不同 owner 的会话 ⇒ 不挑、不合并，
/// 落 `unknown`（anchor 没有 source_agent 列）+ private，并写一条 memory_op 与计数。
#[test]
fn a_row_whose_provenance_spans_two_owners_falls_back_to_the_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let a = seed_session(&mut root, "s-a", "dev-a", "qoder", &[("甲轮", 100)]);
        let b = seed_session(&mut root, "s-b", "dev-b", "hermes", &[("乙轮", 200)]);
        own_session(
            &root,
            "s-a",
            "dev-a",
            Declared { principal: None, agent: "alice", scope_key: "proj-a", visibility: "project", retention: "keep" },
        );
        own_session(
            &root,
            "s-b",
            "dev-b",
            Declared { principal: None, agent: "bob", scope_key: "proj-b", visibility: "project", retention: "keep" },
        );
        anchor_row(&root, "anchor-x", "project", "proj-x", 700);
        prov(&root, "anchor", "anchor-x", &a[0]);
        prov(&root, "anchor", "anchor-x", &b[0]);
    }
    let report = backfill(&data);
    assert_eq!(report["rows_scanned"], 1);
    assert_eq!(report["owners_written"], 1);
    assert_eq!(report["unresolved"], 1);
    assert_eq!(
        owner_of(&data, "anchor", "anchor-x"),
        Some(Owner {
            // 跨身份 ⇒ principal 与 owner 都不许从两边挑一个。
            principal: None,
            agent: "unknown".into(),
            created_by: "unknown".into(),
            visibility: "private_agent".into(),
            retention: "default".into(),
            scope_kind: "project".into(),
            scope_key: "proj-x".into(),
            source_agent: None,
            device_id: None,
            created_at_ms: 700,
        })
    );
    assert_eq!(
        memory_op_count(&data, "anchor", "anchor-x", "owner_backfill_unresolved"),
        1,
        "未决行必须留一条 NOOP 记账，且只有一条"
    );
    assert!(
        unresolved_counter(&data) >= 1,
        "未决数必须进 maintenance:ownership 的 ownership_unresolved"
    );
}

/// ③ 无出处：card 没有任何 turn 出处 ⇒ 未决。
/// 哨兵是 `行.source_agent`（card 有该列），只有 anchor 才是字面量 `unknown`
/// —— 设计 §6 的 `COALESCE(行.source_agent,'unknown')`。
#[test]
fn a_card_without_provenance_gets_the_unresolved_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        card_row(&root, "card-orphan", "entity", "concept-x", "writer", "dev-mem", 600);
    }
    let report = backfill(&data);
    assert_eq!(report["rows_scanned"], 1);
    assert_eq!(report["owners_written"], 1);
    assert_eq!(report["unresolved"], 1);
    assert_eq!(
        owner_of(&data, "card", "card-orphan"),
        Some(Owner {
            principal: None,
            agent: "writer".into(),
            created_by: "writer".into(),
            visibility: "private_agent".into(),
            retention: "default".into(),
            // 行 scope 原样归一：entity 不是契约五值，但它是记忆行自己的词表。
            scope_kind: "entity".into(),
            scope_key: "concept-x".into(),
            source_agent: Some("writer".into()),
            device_id: Some("dev-mem".into()),
            created_at_ms: 600,
        })
    );
    assert_eq!(
        memory_op_count(&data, "card", "card-orphan", "owner_backfill_unresolved"),
        1
    );
    assert_eq!(unresolved_counter(&data), 1);
}

/// ④ 孤儿：`(card,'ghost')` 的归属行在事实表里没有对应行 ⇒ 删掉；
/// 只清 card/anchor 两张表，其它表的归属行（今天是脏数据）不动。
#[test]
fn an_owner_row_without_its_memory_row_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let uids = seed_session(&mut root, "s-a", "dev-a", "qoder", &[("卡正文", 100)]);
        card_row(&root, "card-live", "project", "proj-row", "writer", "dev-mem", 900);
        prov(&root, "card", "card-live", &uids[0]);
        synthetic_owner(&root, "card", "ghost");
        synthetic_owner(&root, "fact", "fact-ghost");
    }
    let report = backfill(&data);
    assert_eq!(report["orphans_removed"], 1, "只该删 card 的那条孤儿");
    assert_eq!(report["owners_written"], 1);
    assert_eq!(owner_of(&data, "card", "ghost"), None, "孤儿归属行没被删");
    assert!(
        owner_of(&data, "fact", "fact-ghost").is_some(),
        "孤儿清理只许扫 card/anchor 两张表"
    );
    assert!(
        owner_of(&data, "card", "card-live").is_some(),
        "有事实行的那条归属不许跟着删"
    );
}

/// ⑤ 幂等：第二次 `written=0 / unresolved=0 / orphans_removed=0`，
/// 归属行逐列不变、memory_op 不重写、计数不翻倍。
#[test]
fn a_second_run_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let uids = seed_session(&mut root, "s-a", "dev-a", "qoder", &[("卡正文", 100)]);
        own_session(
            &root,
            "s-a",
            "dev-a",
            Declared { principal: Some("p1"), agent: "alice", scope_key: "proj-x", visibility: "project", retention: "keep" },
        );
        card_row(&root, "card-1", "project", "proj-row", "writer", "dev-mem", 900);
        prov(&root, "card", "card-1", &uids[0]);
        // 外加一条推不出的行：第二次也不许再记一次账。
        anchor_row(&root, "anchor-np", "project", "proj-x", 700);
    }
    let first = backfill(&data);
    assert_eq!(first["owners_written"], 2);
    assert_eq!(first["unresolved"], 1);
    let digest_after_first = owner_rows_digest(&data);

    let second = backfill(&data);
    assert_eq!(second["rows_scanned"], 2);
    assert_eq!(second["already_owned"], 2);
    assert_eq!(second["owners_written"], 0);
    assert_eq!(second["unresolved"], 0, "已有归属的行不再判未决");
    assert_eq!(second["orphans_removed"], 0);
    assert_eq!(owner_rows_digest(&data), digest_after_first, "归属行被第二次运行改了");
    assert_eq!(
        memory_op_count(&data, "anchor", "anchor-np", "owner_backfill_unresolved"),
        1,
        "memory_op 不许重复写"
    );
    assert_eq!(unresolved_counter(&data), 1, "计数不许第二次再加一遍");
}

/// ⑥ legacy 旧会话（7 列全 NULL）⇒ owner 退回 `source_agent`、private、default，principal 为空。
#[test]
fn a_legacy_session_without_ownership_columns_falls_back_to_source_agent() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let uids = seed_session(&mut root, "s-legacy", "dev-a", "qoder", &[("旧轮", 100)]);
        forget_session_ownership(&root, "s-legacy", "dev-a");
        card_row(&root, "card-legacy", "session", "dev-a/s-legacy", "writer", "dev-mem", 900);
        prov(&root, "card", "card-legacy", &uids[0]);
    }
    let report = backfill(&data);
    assert_eq!(report["owners_written"], 1);
    assert_eq!(report["unresolved"], 0, "legacy 会话是可推导的，不算未决");
    assert_eq!(
        owner_of(&data, "card", "card-legacy"),
        Some(Owner {
            principal: None,
            agent: "qoder".into(),
            created_by: "writer".into(),
            // COALESCE(visibility,'private_agent')
            visibility: "private_agent".into(),
            // COALESCE(retention,'default')
            retention: "default".into(),
            // 契约五值 session 在记忆行词表里归一成 project。
            scope_kind: "project".into(),
            scope_key: "dev-a/s-legacy".into(),
            source_agent: Some("writer".into()),
            device_id: Some("dev-mem".into()),
            created_at_ms: 900,
        })
    );
}

/// ⑦ 同一身份的两个会话：可见性取**最保守**的一档；retention 不一致 ⇒ default；
/// 行 scope_key 为空时退到会话的。
#[test]
fn the_most_conservative_session_visibility_wins_and_disagreeing_retention_falls_back() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let a = seed_session(&mut root, "s-a", "dev-a", "qoder", &[("甲轮", 100)]);
        let b = seed_session(&mut root, "s-b", "dev-b", "qoder", &[("乙轮", 200)]);
        own_session(
            &root,
            "s-a",
            "dev-a",
            Declared { principal: None, agent: "alice", scope_key: "proj-shared", visibility: "project", retention: "keep" },
        );
        own_session(
            &root,
            "s-b",
            "dev-b",
            Declared { principal: None, agent: "alice", scope_key: "proj-shared", visibility: "person", retention: "days:7" },
        );
        card_row(&root, "card-mixed", "project", "", "writer", "dev-mem", 900);
        prov(&root, "card", "card-mixed", &a[0]);
        prov(&root, "card", "card-mixed", &b[0]);
    }
    let report = backfill(&data);
    assert_eq!(report["unresolved"], 0, "同一身份的多会话不算未决");
    let owner = owner_of(&data, "card", "card-mixed").expect("没写归属行");
    assert_eq!(owner.agent, "alice");
    assert_eq!(owner.visibility, "person", "可见性必须取更保守的一档");
    assert_eq!(owner.retention, "default", "会话间 retention 不一致 ⇒ default");
    assert_eq!(owner.scope_key, "proj-shared", "行 scope_key 为空 ⇒ 会话的");
}

/// 空库：文本形状是运维面的契约（字段名与顺序都别动）。
#[test]
fn an_empty_store_reports_zeroes_in_text_form() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    Root::open(&data).unwrap();
    let dd = data_arg(&data);
    let out = run(&["ownership", "backfill", "--data-dir", &dd]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "空库回填必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "ownership backfill: scanned=0 already=0 written=0 unresolved=0 orphans_removed=0"
    );
}

/// 打不开的 data-dir 走现有 operational 路径：非 0、stdout 为空（不编一份报告出来）。
#[test]
fn an_unopenable_store_exits_nonzero_with_nothing_on_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("not-a-dir");
    std::fs::write(&file, b"x").unwrap();
    let out = run(&["ownership", "backfill", "--data-dir", file.to_str().unwrap(), "--json"]);
    assert_ne!(out.status.code(), Some(0), "打不开的库不许当成功");
    assert!(out.stdout.is_empty(), "失败时 stdout 必须为空");
}
