//! 身份层 I1：`session_principal_claim` 的追加语义、解析与导入自动盖章
//! （设计 `2026-10-27-principal-identity.md` §3.1/§4，工单 `2026-10-28-principal-i1` 判据 2）。
//!
//! 这一层断五件事：
//! 1. 连盖两章 ⇒ `resolve_principal` 取最新，旧行仍在（**追加，不覆盖**）；
//! 2. `correction` 不带 reason ⇒ 拒绝；带 reason ⇒ `supersedes` 指回上一行；
//! 3. 无 claim 回落 `sessions.owner_principal_id`；两者都没有 ⇒ NULL（**不造** `unknown` 行）；
//! 4. 导入带 `owner_principal_id` ⇒ 自动落一条 claim（`claimed_by` = 该会话的 `source_agent`），
//!    重传不长第二行，且 `sessions.owner_principal_id` 由 claim 同步写上（P1-7 的镜像列）；
//! 5. 旧库（没有这张表、`user_version` 更老）开库后表存在且查询为空，不报错。
//!
//! 正反例只用通用占位名（小明/acct-N），不含任何语料专名。

use rc_contract::{
    compute_body_fp, compute_turn_uid, normalize, parse, ExchangeFile, NormalizedTurn, SessionId,
    TurnTier,
};
use rc_ledger::transcript::{
    claim_principal, principal_claims, resolve_principal, NewPrincipalClaim, PrincipalClaim,
};
use rc_store::{Root, StoreError};
use serde_json::json;

const NOW: i64 = 1_700_000_000_000;

fn session(key: &str, device: &str) -> SessionId {
    SessionId {
        session_key: key.into(),
        device_id: device.into(),
        source_agent: "qoder".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    }
}

/// 一个会话 + 一轮。走生产唯一写入口（`insert_batch`），让 `sessions` 行真的在库里。
fn seed(root: &mut Root, key: &str, device: &str) {
    let body = "先把这个会话落进转录层";
    let turn = NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: compute_turn_uid(device, key, "human", body, NOW),
        body: body.into(),
        body_fp: compute_body_fp(body),
        role: "human".into(),
        created_at_ms: NOW,
    };
    rc_ledger::insert_batch(root.transcript_mut(), &session(key, device), &[turn], 10).unwrap();
}

/// 一条最朴素的章（`human_claim` / 无名 / 无原因）；变体直接调 `claim_principal`。
fn claim_at(
    root: &Root,
    key: &str,
    device: &str,
    principal: &str,
    now_ms: i64,
) -> Result<PrincipalClaim, StoreError> {
    claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: key,
            device_id: device,
            principal_id: principal,
            principal_name: None,
            source: "human_claim",
            claimed_by: "qoder",
            reason: None,
        },
        now_ms,
    )
}

fn claim_count(root: &Root) -> i64 {
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM session_principal_claim", [], |r| r.get(0))
        .unwrap()
}

/// 镜像列：行不存在与列是 NULL 在这里都读成 `None`（本文件只断"写没写"，不断"为什么没有"）。
fn session_principal_column(root: &Root, key: &str, device: &str) -> Option<String> {
    root.transcript()
        .conn()
        .query_row(
            "SELECT owner_principal_id FROM sessions WHERE session_key=?1 AND device_id=?2",
            [key, device],
            |r| r.get(0),
        )
        .ok()
        .flatten()
}

/// 归属来源那一格（四值闭集 `v3|default|operator|legacy`）。行不存在 ⇒ `None`（`query_row`
/// 会报 `QueryReturnedNoRows`，这里用可选查询把它折成"没有"）。
fn session_ownership_src(root: &Root, key: &str, device: &str) -> Option<String> {
    root.transcript()
        .conn()
        .query_row(
            "SELECT ownership_src FROM sessions WHERE session_key=?1 AND device_id=?2",
            [key, device],
            |r| r.get(0),
        )
        .ok()
        .flatten()
}

#[test]
fn a_second_claim_appends_and_resolve_takes_the_latest() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, "ses-A", "dev-1");

    let first = claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: "ses-A",
            device_id: "dev-1",
            principal_id: "acct-1",
            principal_name: Some("小明"),
            source: "human_claim",
            claimed_by: "qoder",
            reason: None,
        },
        NOW,
    )
    .unwrap();
    let second = claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: "ses-A",
            device_id: "dev-1",
            principal_id: "acct-2",
            principal_name: Some("小明（工作号）"),
            source: "correction",
            claimed_by: "hub",
            reason: Some("上一章盖错了账号"),
        },
        NOW, // 同一毫秒也必须是后写的那条赢（主键含时刻，写侧顶到上一行之后）
    )
    .unwrap();

    // 读侧取最新一条。
    let resolved = resolve_principal(root.transcript(), "ses-A", "dev-1")
        .unwrap()
        .expect("有 claim 就必须解析出身份");
    assert_eq!(resolved.principal_id, "acct-2");
    assert_eq!(resolved.principal_name.as_deref(), Some("小明（工作号）"));
    let provenance = resolved.claim.expect("这一格必须说明章来自 claim 表");
    assert_eq!(provenance.id, second.id);
    assert_eq!(provenance.source, "correction");

    // 追加，不覆盖：两行都在，旧行逐格没被改写。
    let rows = principal_claims(root.transcript(), Some("ses-A")).unwrap();
    assert_eq!(rows.len(), 2, "两章两行");
    assert_eq!(rows[0].id, second.id, "最新在前");
    assert_eq!(rows[1].id, first.id);
    assert_eq!(rows[1].principal_id, "acct-1");
    assert_eq!(rows[1].principal_name.as_deref(), Some("小明"));
    assert_eq!(rows[1].source, "human_claim");
    assert!(rows[0].claimed_at_ms > rows[1].claimed_at_ms, "同毫秒的第二章被顶到后面");
    assert_eq!(claim_count(&root), 2);
}

#[test]
fn a_correction_without_a_reason_is_refused_and_a_reasoned_one_supersedes() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, "ses-A", "dev-1");
    let first = claim_at(&root, "ses-A", "dev-1", "acct-1", NOW).unwrap();

    let refused = claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: "ses-A",
            device_id: "dev-1",
            principal_id: "acct-2",
            principal_name: None,
            source: "correction",
            claimed_by: "hub",
            reason: None,
        },
        NOW,
    );
    assert!(refused.is_err(), "correction 不带 reason 必须被拒");
    // 空白 reason 与没给是同一件事。
    let blank = claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: "ses-A",
            device_id: "dev-1",
            principal_id: "acct-2",
            principal_name: None,
            source: "correction",
            claimed_by: "hub",
            reason: Some("   "),
        },
        NOW,
    );
    assert!(blank.is_err(), "空白 reason 不是原因");
    assert_eq!(claim_count(&root), 1, "被拒的章一行都不许落");

    // source 闭集：闭集外的来源不是"另起一行"，是错误。
    let outside = claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: "ses-A",
            device_id: "dev-1",
            principal_id: "acct-2",
            principal_name: None,
            source: "guess",
            claimed_by: "qoder",
            reason: None,
        },
        NOW,
    );
    assert!(outside.is_err(), "source 必须在闭集里");
    assert_eq!(claim_count(&root), 1);

    let corrected = claim_principal(
        root.transcript(),
        NewPrincipalClaim {
            session_key: "ses-A",
            device_id: "dev-1",
            principal_id: "acct-2",
            principal_name: None,
            source: "correction",
            claimed_by: "hub",
            reason: Some("设备换了主人"),
        },
        NOW,
    )
    .unwrap();
    assert_eq!(corrected.supersedes, Some(first.id), "supersedes 指回被覆盖的那一行");
    assert_eq!(corrected.reason.as_deref(), Some("设备换了主人"));
    assert_eq!(
        resolve_principal(root.transcript(), "ses-A", "dev-1")
            .unwrap()
            .unwrap()
            .principal_id,
        "acct-2"
    );
}

#[test]
fn without_a_claim_resolve_falls_back_to_the_session_column_then_to_null() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, "ses-A", "dev-1");

    // 两者都没有 ⇒ NULL。不是 `unknown`，也不造行。
    assert!(resolve_principal(root.transcript(), "ses-A", "dev-1").unwrap().is_none());
    assert_eq!(claim_count(&root), 0, "没身份就是没身份，不造 unknown 行");

    // 只有 `sessions` 那一列（旧锚）⇒ 回落它。
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='legacy-p'
             WHERE session_key='ses-A' AND device_id='dev-1'",
            [],
        )
        .unwrap();
    let fallback = resolve_principal(root.transcript(), "ses-A", "dev-1")
        .unwrap()
        .expect("会话列有值就必须回落出身份");
    assert_eq!(fallback.principal_id, "legacy-p");
    assert!(fallback.claim.is_none(), "回落的身份没有章行（不许把它冒充成一条 claim）");
    assert_eq!(fallback.principal_name, None);
    assert_eq!(claim_count(&root), 0, "回落不落库");

    // 有章时章压过会话列。
    let claimed = claim_at(&root, "ses-A", "dev-1", "acct-9", NOW).unwrap();
    let resolved = resolve_principal(root.transcript(), "ses-A", "dev-1").unwrap().unwrap();
    assert_eq!(resolved.principal_id, "acct-9");
    assert_eq!(resolved.claim.map(|c| c.id), Some(claimed.id));

    // 没有会话行的键同理：查无 ⇒ None（不是错误）。
    assert!(resolve_principal(root.transcript(), "ses-missing", "dev-1").unwrap().is_none());
}

/// P1-7：落章同步 `sessions` 的归属两列（claim 表是真相，这一列是镜像；现网曾出现
/// claim 4 行而 sessions 4/4 空，doctor/归属面按旧列读就会误报无主）。
///
/// 三支都要在：
/// 1. 先有会话行、再 claim ⇒ 列被写上（`ownership_src='operator'`）；
/// 2. 重放同一条 claim（早退分支）⇒ 列仍被写上——夹具先手动清空该列，否则早退不写也看不出来；
/// 3. 会话行不存在 ⇒ claim 成功、0 行更新、不报错（导入顺序允许 claim 先落）。
#[test]
fn a_claim_syncs_the_session_ownership_columns() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, "ses-sync", "dev-1");

    // 1. 正常落章 ⇒ 镜像列被写上。
    claim_at(&root, "ses-sync", "dev-1", "acct-7", NOW).unwrap();
    assert_eq!(session_principal_column(&root, "ses-sync", "dev-1"), Some("acct-7".into()));
    assert_eq!(session_ownership_src(&root, "ses-sync", "dev-1"), Some("operator".into()));
    assert_eq!(claim_count(&root), 1);

    // 2. 重放同一条章：夹具先把镜像列清空，再走早退分支。
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id=NULL, ownership_src='default'
             WHERE session_key='ses-sync' AND device_id='dev-1'",
            [],
        )
        .unwrap();
    claim_at(&root, "ses-sync", "dev-1", "acct-7", NOW + 1).unwrap();
    assert_eq!(claim_count(&root), 1, "重放不长第二行（早退分支）");
    assert_eq!(
        session_principal_column(&root, "ses-sync", "dev-1"),
        Some("acct-7".into()),
        "早退分支没有同步镜像列"
    );
    assert_eq!(session_ownership_src(&root, "ses-sync", "dev-1"), Some("operator".into()));

    // 3. 没有会话行 ⇒ 0 行更新，章照落，不报错。
    let orphan = claim_at(&root, "ses-no-row", "dev-1", "acct-8", NOW).unwrap();
    assert_eq!(orphan.principal_id, "acct-8");
    assert_eq!(claim_count(&root), 2);
    assert_eq!(session_principal_column(&root, "ses-no-row", "dev-1"), None);
}

#[test]
fn an_import_carrying_a_principal_stamps_a_claim_attributed_to_the_source_agent() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    let file: ExchangeFile = parse(
        &serde_json::to_vec(&json!({
            "format_version": 1,
            "sessions": [{
                "session_key": "ses-imp", "device_id": "dev-imp", "source_agent": "qoder-cli",
                "owner_principal_id": "acct-42", "principal_name": "小明",
                "principal_source": "human_claim",
                "turns": [{"role": "human", "body": "导入时就带上身份", "created_at_ms": 1730000000000i64}]
            }, {
                "session_key": "ses-default", "device_id": "dev-imp", "source_agent": "qoder-cli",
                "owner_principal_id": "acct-43",
                "turns": [{"role": "human", "body": "没写 source 的会话", "created_at_ms": 1730000000001i64}]
            }, {
                "session_key": "ses-anon", "device_id": "dev-imp", "source_agent": "qoder-cli",
                "turns": [{"role": "human", "body": "根本没带身份的会话", "created_at_ms": 1730000000002i64}]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
    let rep = normalize(file, 1);
    assert_eq!(rep.accepted.len(), 3);

    let mut accepted = rep.accepted.clone();
    accepted.sort_by(|a, b| a.session.session_key.cmp(&b.session.session_key));
    for acc in &accepted {
        rc_ledger::transcript::insert_batch_committed(
            root.transcript_mut(),
            &acc.session,
            std::slice::from_ref(&acc.turn),
            NOW,
            "import",
        )
        .unwrap();
    }

    let claims = principal_claims(root.transcript(), Some("ses-imp")).unwrap();
    assert_eq!(claims.len(), 1, "导入带身份 ⇒ 自动落一条章");
    assert_eq!(claims[0].principal_id, "acct-42");
    assert_eq!(claims[0].principal_name.as_deref(), Some("小明"));
    assert_eq!(claims[0].source, "human_claim", "source 取 principal_source");
    assert_eq!(claims[0].claimed_by, "qoder-cli", "claimed_by = 该会话的 source_agent");
    // `sessions` 那一列由 claim 同步写上（P1-7：章表是真相，这一列是镜像；两列不一致会让
    // doctor/归属面按旧列误报"无主"）。
    assert_eq!(
        session_principal_column(&root, "ses-imp", "dev-imp"),
        Some("acct-42".into())
    );
    assert_eq!(session_ownership_src(&root, "ses-imp", "dev-imp"), Some("operator".into()));
    assert_eq!(
        resolve_principal(root.transcript(), "ses-imp", "dev-imp").unwrap().unwrap().principal_id,
        "acct-42"
    );

    // 缺省 source ⇒ agent_runtime；不带身份 ⇒ 一条章都不落（不造 unknown）。
    let default_source = principal_claims(root.transcript(), Some("ses-default")).unwrap();
    assert_eq!(default_source.len(), 1);
    assert_eq!(default_source[0].source, "agent_runtime");
    assert!(principal_claims(root.transcript(), Some("ses-anon")).unwrap().is_empty());
    assert!(resolve_principal(root.transcript(), "ses-anon", "dev-imp").unwrap().is_none());

    // 重传同一份文件：不长第二行（设计 §7 第 2 条的幂等）。
    for acc in &accepted {
        rc_ledger::transcript::insert_batch_committed(
            root.transcript_mut(),
            &acc.session,
            std::slice::from_ref(&acc.turn),
            NOW + 5_000,
            "import",
        )
        .unwrap();
    }
    assert_eq!(principal_claims(root.transcript(), Some("ses-imp")).unwrap().len(), 1);
    assert_eq!(claim_count(&root), 2, "两个带身份��会话各一行");
}

#[test]
fn an_old_database_without_the_claim_table_heals_on_open_and_reads_empty() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut root = Root::open(dir.path()).unwrap();
        seed(&mut root, "ses-old", "dev-old");
        // 模拟"这张表还不存在"的旧库：删表删索引、把版本退到 I1 之前。
        root.transcript()
            .conn()
            .execute_batch(
                "DROP INDEX claim_by_session;
                 DROP TABLE session_principal_claim;
                 PRAGMA user_version=4;",
            )
            .unwrap();
    }
    // 旧库再开：`apply` 每次重放 `CREATE TABLE IF NOT EXISTS` ⇒ 表回来，查询为空，不报错。
    let root = Root::open(dir.path()).unwrap();
    let n: i64 = root
        .transcript()
        .conn()
        .query_row("SELECT count(*) FROM session_principal_claim", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    assert!(resolve_principal(root.transcript(), "ses-old", "dev-old").unwrap().is_none());
    // 表是真的能写（不只是"存在"）。
    claim_at(&root, "ses-old", "dev-old", "acct-old", NOW).unwrap();
    assert_eq!(claim_count(&root), 1);
}

/// 判据 5 的只读读数：章在库里（不是内存里造的）。带 `--nocapture` 跑时打印两格。
#[test]
fn the_readout_comes_from_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let mut root = Root::open(dir.path()).unwrap();
    seed(&mut root, "ses-A", "dev-1");
    claim_at(&root, "ses-A", "dev-1", "acct-1", NOW).unwrap();
    claim_at(&root, "ses-A", "dev-1", "acct-2", NOW + 1).unwrap();

    let resolved = resolve_principal(root.transcript(), "ses-A", "dev-1").unwrap().unwrap();
    println!(
        "SELECT count(*) FROM session_principal_claim = {}; resolve_principal = {:?}",
        claim_count(&root),
        resolved.principal_id
    );
    assert_eq!(claim_count(&root), 2);
    assert_eq!(resolved.principal_id, "acct-2");
}
