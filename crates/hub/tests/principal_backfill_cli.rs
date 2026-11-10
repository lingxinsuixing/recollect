// crates/hub/tests/principal_backfill_cli.rs
//! W37-A2 / G13④：`hub principal backfill` 的 CLI 面（设计 §4"存量 226 行处置"）。
//!
//! 判定面是**真进程的 stdout + 库里的行**：dry-run 零写入、apply 真写带账、幂等重跑、
//! 未决行不挑不猜，只有这两样看得见。
//!
//! 口径（与 `hub::principal_backfill` 的实现同一句话）：
//! - 主人**只认章**：`provenance.turn_uid → turns → 会话` 经 `resolve_principal`，
//!   `claim.is_some()` 且过 `principal_is_known` 才算——列上的回落值就是这轮要补的缺口；
//! - 恰好一个身份才回填；无章 / 跨 ≥2 个章的行原样留着（`unresolved`）；
//! - 每写一行归属，同一事务写一条
//!   `memory_op(op='UPDATE', reason='principal_backfill', actor='program')`（before/after）；
//! - **默认 dry-run，一格不写**（变异钉：让 dry-run 也写 ⇒ `dry_run_writes_nothing` 红）。
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::{claim_principal, insert_batch, NewPrincipalClaim};
use rc_store::Root;
use rusqlite::params;
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
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
/// 走生产唯一写入口 `insert_batch`；会话的归属列保持 NULL——生产缺章 226 行的形状。
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

/// 给会话盖一条章，然后把镜像列抹回 NULL：模拟"章后补、列还是 NULL"的存量库
/// （P1-7 镜像同步落地之前的行）。回填的判据只读章，列状态不影响判定，
/// 抹列是为了让夹具与"缺章行"的成因同形。
fn stamp_then_wipe(root: &Root, key: &str, device: &str, principal: &str, source: &str) {
    claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: key,
            device_id: device,
            principal_id: principal,
            principal_name: Some("旧主人"),
            source,
            claimed_by: "env:RECOLLECT_PRINCIPAL_NAME",
            reason: None,
        },
        20_000,
    )
    .unwrap();
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=NULL
              WHERE session_key=?1 AND device_id=?2",
            params![key, device],
        )
        .unwrap();
}

/// 一条缺章归属行（主人 NULL，其余与 `resource_owner` 的 NOT NULL 对齐）。
fn missing_card(root: &Root, id: &str, scope_key: &str, agent: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO card(id, kind, title, text, scope_kind, scope_key, as_of_ms,
                              source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1, 'fact', ?1, '合成卡正文', 'project', ?2, 900, ?3, 'dev-mem', 900, 1)",
            params![id, scope_key, agent],
        )
        .unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('card', ?1, NULL, ?2, ?2, 'private_agent', 'default', 'project', ?3, ?2, 'dev-mem', 900)",
            params![id, agent, scope_key],
        )
        .unwrap();
}

fn prov(root: &Root, mem_id: &str, turn_uid: &str) {
    prov_for(root, "card", mem_id, turn_uid)
}

/// 参数化出处：backfill 扫描面扩到 fact/state_assertion 后，出处同样分表。
fn prov_for(root: &Root, mem_table: &str, mem_id: &str, turn_uid: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO provenance(mem_table, mem_id, turn_uid, anchor_role)
             VALUES(?1, ?2, ?3, 'human')",
            params![mem_table, mem_id, turn_uid],
        )
        .unwrap();
}

/// 一条缺章 fact（主体锚指向一个已有归属的锚——fact.subject_anchor_id NOT NULL）。
fn missing_fact(root: &Root, id: &str, subject: &str, scope_key: &str, agent: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO fact(id, subject_anchor_id, predicate, category, mode, text,
                              evidence_turn_uid, scope_kind, scope_key,
                              source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1, ?2, '看到了', 'observation', 'stated', '合成事实正文',
                    'turn-x', 'project', ?3, ?4, 'dev-mem', 900, 1)",
            params![id, subject, scope_key, agent],
        )
        .unwrap();
    owner_row(root, "fact", id, scope_key, agent);
}

/// 一条缺章 state_assertion（subject_kind='anchor' 臂）。
fn missing_state(root: &Root, id: &str, subject: &str, scope_key: &str, agent: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO state_assertion(id, subject_kind, subject_anchor_id, dimension,
                                         value_after, text, mode,
                                         scope_kind, scope_key,
                                         source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1, 'anchor', ?2, 'mood', '平静', '合成状态正文', 'expressed',
                    'project', ?3, ?4, 'dev-mem', 900, 1)",
            params![id, subject, scope_key, agent],
        )
        .unwrap();
    owner_row(root, "state_assertion", id, scope_key, agent);
}

/// resource_owner 缺章行（主人 NULL），四表同形。
fn owner_row(root: &Root, mem_table: &str, mem_id: &str, scope_key: &str, agent: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES(?1, ?2, NULL, ?3, ?3, 'private_agent', 'default', 'project', ?4, ?3, 'dev-mem', 900)",
            params![mem_table, mem_id, agent, scope_key],
        )
        .unwrap();
}

// ───────────────────────────── 读数 ─────────────────────────────

fn principal_of(data: &Path, mem_table: &str, mem_id: &str) -> Option<String> {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row(
            "SELECT owner_principal_id FROM resource_owner WHERE mem_table=?1 AND mem_id=?2",
            params![mem_table, mem_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// `memory_op` 里 reason='principal_backfill' 的行数（不带 op 过滤：记账面只认这一个 reason）。
fn ledger_rows(data: &Path, mem_id: &str) -> Vec<(String, String, String)> {
    ledger_rows_for(data, "card", mem_id)
}

fn ledger_rows_for(data: &Path, mem_table: &str, mem_id: &str) -> Vec<(String, String, String)> {
    let root = Root::open(data).unwrap();
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT op, IFNULL(before_json,'<null>'), IFNULL(after_json,'<null>')
               FROM memory_op WHERE mem_table=?2 AND mem_id=?1
                AND reason='principal_backfill' AND actor='program'
              ORDER BY created_at_ms, id",
        )
        .unwrap();
    st.query_map(params![mem_id, mem_table], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn claim_count(data: &Path, session_key: &str) -> i64 {
    let root = Root::open(data).unwrap();
    root.transcript()
        .conn()
        .query_row(
            "SELECT count(*) FROM session_principal_claim WHERE session_key=?1",
            params![session_key],
            |r| r.get(0),
        )
        .unwrap()
}

/// 跑一次 `principal backfill --json`，失败时把 stderr 带进断言消息。
fn backfill(data: &Path, apply: bool) -> Value {
    let dd = data_arg(data);
    let mut args = vec!["principal", "backfill", "--data-dir", &dd, "--json"];
    if apply {
        args.push("--apply");
    }
    let out = run(&args);
    assert_eq!(
        out.status.code(),
        Some(0),
        "回填必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}

// ───────────────────────────── 用例 ─────────────────────────────

/// 变异钉（交付判据②）：dry-run 必须**零写入**——归属不动、`memory_op` 一条不落。
/// 把实现里 dry-run 分支改成也执行（变异证明 B）⇒ 本条当场红。
#[test]
fn dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let uids = seed_session(&mut root, "s-a", "dev-a", "qoder", &[("卡正文", 100)]);
        stamp_then_wipe(&root, "s-a", "dev-a", "p-claim", "agent_runtime");
        missing_card(&root, "card-1", "proj-x", "writer");
        prov(&root, "card-1", &uids[0]);
    }
    let report = backfill(&data, false);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["rows_scanned"], 1);
    assert_eq!(report["backfilled"], 0, "dry-run 的 backfilled 恒 0");
    assert_eq!(report["unresolved"], 0);
    // 回执照给：rows 列的是"将会写什么"。
    assert_eq!(report["rows"][0]["mem_table"], "card");
    assert_eq!(report["rows"][0]["mem_id"], "card-1");
    assert_eq!(report["rows"][0]["principal_id"], "p-claim");

    // 库面：零写入（交付判据的字面形状）。
    assert_eq!(principal_of(&data, "card", "card-1"), None, "dry-run 不许动归属");
    assert!(ledger_rows(&data, "card-1").is_empty(), "dry-run 不许记账");
    assert_eq!(claim_count(&data, "s-a"), 1, "dry-run 不许碰章表");
}

/// apply：缺章行从**章**上补回主人，同一事务里留 `memory_op`（before=NULL / after=p-claim）。
#[test]
fn apply_backfills_from_the_claim_and_keeps_the_ledger() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let uids = seed_session(&mut root, "s-a", "dev-a", "qoder", &[("卡正文", 100)]);
        stamp_then_wipe(&root, "s-a", "dev-a", "p-claim", "agent_runtime");
        missing_card(&root, "card-1", "proj-x", "writer");
        prov(&root, "card-1", &uids[0]);
    }
    let report = backfill(&data, true);
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["rows_scanned"], 1);
    assert_eq!(report["backfilled"], 1);
    assert_eq!(report["unresolved"], 0);

    assert_eq!(principal_of(&data, "card", "card-1").as_deref(), Some("p-claim"));
    let rows = ledger_rows(&data, "card-1");
    assert_eq!(rows.len(), 1, "恰好一条带账");
    assert_eq!(rows[0].0, "UPDATE");
    assert_eq!(
        rows[0].1,
        r#"{"owner_principal_id":null}"#,
        "before 记旧值（缺章 = null，不是省键）"
    );
    assert_eq!(rows[0].2, r#"{"owner_principal_id":"p-claim"}"#);
}

/// 幂等：重跑同一库，缺章行集已空 ⇒ 0 扫描、0 写入、`memory_op` 不重写。
#[test]
fn apply_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let uids = seed_session(&mut root, "s-a", "dev-a", "qoder", &[("卡正文", 100)]);
        stamp_then_wipe(&root, "s-a", "dev-a", "p-claim", "agent_runtime");
        missing_card(&root, "card-1", "proj-x", "writer");
        prov(&root, "card-1", &uids[0]);
    }
    let first = backfill(&data, true);
    assert_eq!(first["backfilled"], 1);
    let second = backfill(&data, true);
    assert_eq!(second["rows_scanned"], 0, "回填过的行不再缺章，第二轮扫不到");
    assert_eq!(second["backfilled"], 0);
    assert_eq!(ledger_rows(&data, "card-1").len(), 1, "记账不重写");
}

/// 未决行不挑不猜：无章的会话、跨两个章的行都原样留着（`unresolved` 计数），
/// 主人格一格不动。
#[test]
fn unresolved_rows_are_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        // s-nc：始终无章；s-1/s-2：两个不同的章。
        let nc = seed_session(&mut root, "s-nc", "dev-nc", "qoder", &[("无章正文", 100)]);
        let a = seed_session(&mut root, "s-1", "dev-1", "qoder", &[("甲正文", 110)]);
        let b = seed_session(&mut root, "s-2", "dev-2", "qoder", &[("乙正文", 120)]);
        stamp_then_wipe(&root, "s-1", "dev-1", "p-one", "agent_runtime");
        stamp_then_wipe(&root, "s-2", "dev-2", "p-two", "agent_runtime");
        // card-nc：出处只有无章会话 ⇒ 推不出章。
        missing_card(&root, "card-nc", "proj-x", "writer");
        prov(&root, "card-nc", &nc[0]);
        // card-cross：出处横跨两个章 ⇒ 不挑不合并。
        missing_card(&root, "card-cross", "proj-x", "writer");
        prov(&root, "card-cross", &a[0]);
        prov(&root, "card-cross", &b[0]);
    }
    let report = backfill(&data, true);
    assert_eq!(report["rows_scanned"], 2);
    assert_eq!(report["backfilled"], 0);
    assert_eq!(report["unresolved"], 2);
    assert_eq!(principal_of(&data, "card", "card-nc"), None);
    assert_eq!(principal_of(&data, "card", "card-cross"), None);
    assert!(ledger_rows(&data, "card-nc").is_empty());
    assert!(ledger_rows(&data, "card-cross").is_empty());
}

/// 尾债②（W37 收尾）：扫描面含 fact/state_assertion，且章判定走 correction 链——
/// 现网 53 行缺章 fact/state 的成因形状：会话先被盖了旧串章（`旧主人`），归一化补了
/// correction 章（`p-owner`）。回填认**最新一条章**（与 `resolve_principal` 同口径），
/// 不是把新旧两个章数成"跨 ≥2 个章"判 unresolved。
#[test]
fn backfills_fact_and_state_from_latest_correction_claim() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        let uids = seed_session(&mut root, "s-c", "dev-c", "qoder", &[("状态正文", 130)]);
        // 归一化链的形状：旧章在前，correction 章在后（supersedes 由 claim_on 自动接）。
        for (principal_id, source, claimed_by, reason, now_ms) in [
            ("旧主人", "agent_runtime", "env:RECOLLECT_PRINCIPAL_NAME", None, 20_000),
            ("p-owner", "correction", "hub", Some("身份归一化"), 21_000),
        ] {
            claim_principal(
                root.transcript(),
                NewPrincipalClaim {
                    session_key: "s-c",
                    device_id: "dev-c",
                    principal_id,
                    principal_name: Some("旧主人"),
                    source,
                    claimed_by,
                    reason,
                },
                now_ms,
            )
            .unwrap();
        }
        owned_anchor(&root, "anchor-subj", "anchor-subj", "proj-x", "qoder", "p-owner");
        missing_fact(&root, "fact-1", "anchor-subj", "proj-x", "writer");
        missing_state(&root, "state-1", "anchor-subj", "proj-x", "writer");
        prov_for(&root, "fact", "fact-1", &uids[0]);
        prov_for(&root, "state_assertion", "state-1", &uids[0]);
    }
    let report = backfill(&data, true);
    assert_eq!(report["rows_scanned"], 2);
    assert_eq!(report["backfilled"], 2);
    assert_eq!(report["unresolved"], 0);

    assert_eq!(principal_of(&data, "fact", "fact-1").as_deref(), Some("p-owner"));
    assert_eq!(
        principal_of(&data, "state_assertion", "state-1").as_deref(),
        Some("p-owner")
    );
    let rows = ledger_rows_for(&data, "fact", "fact-1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "UPDATE");
    assert_eq!(rows[0].1, r#"{"owner_principal_id":null}"#);
    assert_eq!(rows[0].2, r#"{"owner_principal_id":"p-owner"}"#);
}

// ───────────────────────── remap（W37 归一化）─────────────────────────

use hub::principal_backfill::remap_principals;

/// 一格带旧归属串的锚（person，canonical=旧 id——昵称当 id 的存量形状）。
fn owned_anchor(root: &Root, id: &str, canonical: &str, scope_key: &str, agent: &str, principal: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                norm_version)
             VALUES(?1, 'person', ?2, 'real', NULL, 'project', ?3, 'active', NULL, 900, 900, 1)",
            params![id, canonical, scope_key],
        )
        .unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('anchor', ?1, ?4, ?2, ?2, 'private_agent', 'default', 'project', ?3, ?2, 'dev-mem', 900)",
            params![id, agent, scope_key, principal],
        )
        .unwrap();
}

/// 一格带旧归属串的卡（resource_owner 覆盖 4 张 mem_table，remap 不限 card/anchor——
/// 这里用 card 代表"非锚表也要被改到"）。
fn owned_card(root: &Root, id: &str, scope_key: &str, agent: &str, principal: &str) {
    root.memory()
        .conn()
        .execute(
            "INSERT INTO card(id, kind, title, text, scope_kind, scope_key, as_of_ms,
                              source_agent, device_id, created_at_ms, norm_version)
             VALUES(?1, 'fact', ?1, '合成卡正文', 'project', ?2, 900, ?3, 'dev-mem', 900, 1)",
            params![id, scope_key, agent],
        )
        .unwrap();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('card', ?1, ?4, ?2, ?2, 'private_agent', 'default', 'project', ?3, ?2, 'dev-mem', 900)",
            params![id, agent, scope_key, principal],
        )
        .unwrap();
}

/// `memory_op` 里 reason='principal_remap' 的记账行。
fn remap_ledger(data: &Path, mem_table: &str, mem_id: &str) -> Vec<(String, String, String)> {
    let root = Root::open(data).unwrap();
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT op, IFNULL(before_json,'<null>'), IFNULL(after_json,'<null>')
               FROM memory_op WHERE mem_table=?1 AND mem_id=?2
                AND reason='principal_remap' AND actor='program'
              ORDER BY created_at_ms, id",
        )
        .unwrap();
    st.query_map(params![mem_table, mem_id], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn person_anchor_of(data: &Path, canonical: &str) -> Option<String> {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row(
            "SELECT id FROM anchor WHERE kind='person' AND canonical=?1",
            params![canonical],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
}

fn alias_exists(data: &Path, anchor_id: &str, alias: &str) -> bool {
    let root = Root::open(data).unwrap();
    root.memory()
        .conn()
        .query_row(
            "SELECT 1 FROM alias WHERE anchor_id=?1 AND alias=?2",
            params![anchor_id, alias],
            |r| r.get::<_, i64>(0),
        )
        .is_ok()
}

fn remap(data: &Path, from: &str, to: &str, name: Option<&str>, apply: bool) -> (i32, Value) {
    let dd = data_arg(data);
    let mut args = vec!["principal", "remap", "--data-dir", &dd, "--json", "--from", from, "--to", to];
    if let Some(n) = name {
        args.extend(["--name", n]);
    }
    if apply {
        args.push("--apply");
    }
    let out = run(&args);
    let code = out.status.code().unwrap_or(-1);
    if code != 0 {
        return (code, Value::Null);
    }
    (code, serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象"))
}

/// 变异钉：dry-run 零写入——归属串不动、不记账、**不建锚**。
#[test]
fn remap_dry_run_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        owned_anchor(&root, "anc-old", "旧主人", "recollect", "qoder", "旧主人");
        owned_card(&root, "card-1", "recollect", "qoder", "旧主人");
    }
    let (code, report) = remap(&data, "旧主人", "p-owner", Some("旧主人"), false);
    assert_eq!(code, 0);
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["rows_matched"], 2);
    assert_eq!(report["remapped"], 0);
    assert_eq!(report["anchors_ensured"], 0, "dry-run 不许建锚");
    assert_eq!(principal_of(&data, "anchor", "anc-old").as_deref(), Some("旧主人"));
    assert_eq!(principal_of(&data, "card", "card-1").as_deref(), Some("旧主人"));
    assert!(remap_ledger(&data, "card", "card-1").is_empty(), "dry-run 不许记账");
    assert!(person_anchor_of(&data, "p-owner").is_none(), "dry-run 不许建锚");
}

/// apply：两格归属串都改写、各留一条 `principal_remap` 记账（before=旧 id / after=新 id）、
/// 新主人锚按旧锚的 scope 确保出来（canonical=新 id、可读名进 alias）、归属形状照抄旧锚。
#[test]
fn remap_apply_rewrites_accounts_and_ensures_anchor() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        owned_anchor(&root, "anc-old", "旧主人", "recollect", "qoder", "旧主人");
        owned_card(&root, "card-1", "recollect", "qoder", "旧主人");
    }
    let (code, report) = remap(&data, "旧主人", "p-owner", Some("旧主人"), true);
    assert_eq!(code, 0);
    assert_eq!(report["dry_run"], false);
    assert_eq!(report["rows_matched"], 2);
    assert_eq!(report["remapped"], 2);
    assert_eq!(report["anchors_ensured"], 1);
    assert_eq!(report["scopes"][0], "project:recollect");

    assert_eq!(principal_of(&data, "anchor", "anc-old").as_deref(), Some("p-owner"));
    assert_eq!(principal_of(&data, "card", "card-1").as_deref(), Some("p-owner"));
    let rows = remap_ledger(&data, "card", "card-1");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "UPDATE");
    assert_eq!(rows[0].1, r#"{"owner_principal_id":"旧主人"}"#);
    assert_eq!(rows[0].2, r#"{"owner_principal_id":"p-owner"}"#);

    let new_id = person_anchor_of(&data, "p-owner").expect("新主人锚必须存在");
    assert!(alias_exists(&data, &new_id, "旧主人"), "可读名进 alias");
    // 归属形状照抄旧锚：owner_agent 不丢（qoder），新锚的归属行指向新 id。
    let root = Root::open(&data).unwrap();
    let (agent, principal): (String, Option<String>) = root
        .memory()
        .conn()
        .query_row(
            "SELECT owner_agent_id, owner_principal_id FROM resource_owner
              WHERE mem_table='anchor' AND mem_id=?1",
            params![new_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(agent, "qoder");
    assert_eq!(principal.as_deref(), Some("p-owner"));
}

/// 幂等：重跑同一库，旧 id 行集已空 ⇒ 0 匹配、0 改写、不重记账；锚已存在 ⇒ 0 确保。
#[test]
fn remap_apply_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        owned_anchor(&root, "anc-old", "旧主人", "recollect", "qoder", "旧主人");
        owned_card(&root, "card-1", "recollect", "qoder", "旧主人");
    }
    let first = remap(&data, "旧主人", "p-owner", Some("旧主人"), true);
    assert_eq!(first.1["remapped"], 1 + 1);
    let second = remap(&data, "旧主人", "p-owner", Some("旧主人"), true);
    assert_eq!(second.1["rows_matched"], 0);
    assert_eq!(second.1["remapped"], 0);
    assert_eq!(second.1["anchors_ensured"], 0, "没有受影响锚行就没有 ensure");
    assert_eq!(remap_ledger(&data, "card", "card-1").len(), 1, "记账不重写");
}

/// 用法错误：`from == to`（或空串）在开库**之前**拒绝（退出 3）。
#[test]
fn remap_rejects_degenerate_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let (code, _) = remap(&data, "旧主人", "旧主人", None, true);
    assert_eq!(code, 3, "from == to 必须退出 3");
    let (code, _) = remap(&data, "", "p-x", None, true);
    assert_eq!(code, 3, "空 from 必须退出 3");
}

/// 库面直调（非 CLI 路径也要有牙）：remap_principals 的 dry-run/apply 与 CLI 同一口径。
#[test]
fn remap_library_path_matches_cli() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        owned_card(&root, "card-1", "recollect", "qoder", "human:uuid-x");
    }
    let root = Root::open(&data).unwrap();
    let (report, _) = remap_principals(&root, "human:uuid-x", "p-owner", None, 1_000, false).unwrap();
    assert_eq!(report.rows_matched, 1);
    assert_eq!(report.remapped, 0);
    let (report, rows) = remap_principals(&root, "human:uuid-x", "p-owner", None, 1_100, true).unwrap();
    assert_eq!(report.remapped, 1);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].mem_table, "card");
    assert_eq!(
        principal_of(&data, "card", "card-1").as_deref(),
        Some("p-owner")
    );
}
