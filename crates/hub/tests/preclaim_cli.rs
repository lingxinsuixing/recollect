// crates/hub/tests/preclaim_cli.rs
//! W37-A2 / G13③②：`hub principal claim-env` 的 CLI 面（daily-drain 蒸馏前的
//! 前置补章那一枪；设计 §4"写入时点修复"）。
//!
//! 判定面是**真进程的 stdout + 库里的行**：声明从进程环境来（与导入路径同一份
//! `PrincipalDecl`），补的章 `source='agent_runtime'`、`claimed_by='env:…'`，
//! 幂等重跑不长第二行，人盖的章不被顶掉。
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::{claim_principal, insert_batch, NewPrincipalClaim};
use rc_store::Root;
use rusqlite::params;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn data_arg(data: &Path) -> String {
    data.to_str().unwrap().to_string()
}

/// 跑一次 `principal claim-env --json`，环境变量由调用方给。
fn claim_env(data: &Path, env: &[(&str, &str)]) -> (Output, Value) {
    let dd = data_arg(data);
    let mut cmd = Command::new(bin());
    cmd.args(["principal", "claim-env", "--data-dir", &dd, "--json"]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("hub binary");
    assert_eq!(
        out.status.code(),
        Some(0),
        "claim-env 必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value =
        serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    (out, v)
}

// ───────────────────────────── 夹具 ─────────────────────────────

fn seed_session(root: &mut Root, key: &str, device: &str, agent: &str, body: &str) -> String {
    let uid = compute_turn_uid(device, key, "human", body, 100);
    let turn = NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: uid.clone(),
        body: body.into(),
        body_fp: compute_body_fp(body),
        role: "human".into(),
        created_at_ms: 100,
    };
    let session = SessionId {
        session_key: key.into(),
        device_id: device.into(),
        source_agent: agent.into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    insert_batch(root.transcript_mut(), &session, std::slice::from_ref(&turn), 10_000).unwrap();
    uid
}

fn stamp(root: &Root, key: &str, device: &str, principal: &str, source: &str) {
    claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: key,
            device_id: device,
            principal_id: principal,
            principal_name: Some("手工"),
            source,
            claimed_by: "hub",
            reason: None,
        },
        20_000,
    )
    .unwrap();
}

fn claim_rows(data: &Path, session_key: &str) -> Vec<(String, String, Option<i64>)> {
    let root = Root::open(data).unwrap();
    let mut st = root
        .transcript()
        .conn()
        .prepare(
            "SELECT principal_id, source, supersedes FROM session_principal_claim
              WHERE session_key=?1 ORDER BY claimed_at_ms",
        )
        .unwrap();
    st.query_map(params![session_key], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<i64>>(2)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn column_of(data: &Path, session_key: &str) -> Option<String> {
    let root = Root::open(data).unwrap();
    root.transcript()
        .conn()
        .query_row(
            "SELECT owner_principal_id FROM sessions WHERE session_key=?1",
            params![session_key],
            |r| r.get(0),
        )
        .unwrap()
}

// ───────────────────────────── 用例 ─────────────────────────────

/// 声明缺席（`RECOLLECT_PRINCIPAL_NAME` 没设）⇒ rc=0、全零读数、一格不写。
/// "没声明"不是故障，更不许在没声明时发明一个主人。
#[test]
fn env_absent_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        seed_session(&mut root, "s-a", "dev-a", "qoder", "无章正文");
    }
    let (_, report) = claim_env(&data, &[]);
    assert_eq!(report["declared"], false);
    assert_eq!(report["sessions_scanned"], 0);
    assert_eq!(report["claims_written"], 0);
    assert_eq!(report["already_claimed"], 0);
    assert_eq!(claim_rows(&data, "s-a").len(), 0, "没声明不许盖章");
    assert_eq!(column_of(&data, "s-a"), None);
}

/// 正例：无章会话按环境声明补章——`source='agent_runtime'`、镜像列同步，
/// 扫描数与写入数对得上。
#[test]
fn stamps_unclaimed_sessions_from_the_env() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        seed_session(&mut root, "s-a", "dev-a", "qoder", "无章正文");
    }
    let (_, report) = claim_env(
        &data,
        &[("RECOLLECT_PRINCIPAL_NAME", "示例主人"), ("RECOLLECT_PRINCIPAL_ID", "p-env")],
    );
    assert_eq!(report["declared"], true);
    assert_eq!(report["sessions_scanned"], 1);
    assert_eq!(report["claims_written"], 1);

    let rows = claim_rows(&data, "s-a");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "p-env");
    assert_eq!(rows[0].1, "agent_runtime", "闭集第一值：环境声明补的章");
    assert_eq!(column_of(&data, "s-a").as_deref(), Some("p-env"), "镜像列同步（P1-7）");
}

/// 幂等：重跑同一库 `claims_written=0`，章链不长第二行。
#[test]
fn rerun_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        seed_session(&mut root, "s-a", "dev-a", "qoder", "无章正文");
    }
    let env = [("RECOLLECT_PRINCIPAL_NAME", "示例主人"), ("RECOLLECT_PRINCIPAL_ID", "p-env")];
    let (_, first) = claim_env(&data, &env);
    assert_eq!(first["claims_written"], 1);
    let (_, second) = claim_env(&data, &env);
    assert_eq!(second["sessions_scanned"], 0, "补过章的会话不再是无章会话");
    assert_eq!(second["claims_written"], 0);
    assert_eq!(claim_rows(&data, "s-a").len(), 1, "重跑不许长第二行");
}

/// 人盖的章不被顶掉：`human_claim` 的会话不在扫描面里，一格不写。
#[test]
fn human_claims_are_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        seed_session(&mut root, "s-a", "dev-a", "qoder", "人盖正文");
        stamp(&root, "s-a", "dev-a", "p-human", "human_claim");
    }
    let (_, report) = claim_env(
        &data,
        &[("RECOLLECT_PRINCIPAL_NAME", "示例主人"), ("RECOLLECT_PRINCIPAL_ID", "p-env")],
    );
    assert_eq!(report["sessions_scanned"], 0);
    assert_eq!(report["claims_written"], 0);
    let rows = claim_rows(&data, "s-a");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "p-human");
    assert_eq!(rows[0].1, "human_claim");
}

/// `unknown` 章（Agent 答的"不知道"）可以补：追加一条 `agent_runtime` 让它 `supersedes`，
/// 历史仍在、镜像列跟到新章。
#[test]
fn an_unknown_claim_gets_superseded() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        seed_session(&mut root, "s-a", "dev-a", "qoder", "不知道正文");
        stamp(&root, "s-a", "dev-a", "unknown", "unknown");
    }
    let (_, report) = claim_env(
        &data,
        &[("RECOLLECT_PRINCIPAL_NAME", "示例主人"), ("RECOLLECT_PRINCIPAL_ID", "p-env")],
    );
    assert_eq!(report["sessions_scanned"], 1);
    assert_eq!(report["claims_written"], 1);
    let rows = claim_rows(&data, "s-a");
    assert_eq!(rows.len(), 2, "追加，不覆盖：unknown 的历史仍在");
    assert_eq!(rows[0].0, "unknown");
    assert_eq!(rows[1].0, "p-env");
    assert_eq!(rows[1].1, "agent_runtime");
    assert_eq!(rows[1].2, Some(1), "新章 supersedes 那条 unknown");
    assert_eq!(column_of(&data, "s-a").as_deref(), Some("p-env"));
}

/// 列上已有已知 id 的会话**不在**补章面里：`resolve_principal` 的回落臂已经能解析它，
/// 用环境声明的 id 顶掉导出器/运维写在列上的身份是替人改归属。
#[test]
fn a_known_column_without_claim_is_out_of_scope() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        seed_session(&mut root, "s-a", "dev-a", "qoder", "列上正文");
        root.transcript()
            .conn()
            .execute(
                "UPDATE sessions SET owner_principal_id='p-exporter'
                  WHERE session_key='s-a' AND device_id='dev-a'",
                [],
            )
            .unwrap();
    }
    let (_, report) = claim_env(
        &data,
        &[("RECOLLECT_PRINCIPAL_NAME", "示例主人"), ("RECOLLECT_PRINCIPAL_ID", "p-env")],
    );
    assert_eq!(report["sessions_scanned"], 0);
    assert_eq!(report["claims_written"], 0);
    assert_eq!(claim_rows(&data, "s-a").len(), 0);
    assert_eq!(column_of(&data, "s-a").as_deref(), Some("p-exporter"), "列上的身份原样");
}
