//! 授权/审计账本快照复制：token 哈希/能力/审计到达备份；同一 commit_seq 的内容变化
//! 由 snapshot_sha 识别；接管后旧设备仍能认证。

use hub::hub_cfg::HubConfig;
use hub::replicate::{apply_ledger, export_ledger, ledger_apply_status, LedgerApplyError};
use hub::serve::HubState;
use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch_committed;
use rc_net::http::{Handler, Request};
use rc_net::key::{DeviceKey, HubKey};
use rc_store::Root;
use serde_json::Value;
use std::process::Command;

const NOW: i64 = 1_000;

fn pair_device(root: &mut Root) -> (DeviceKey, String) {
    let dev = DeviceKey::generate().unwrap();
    let (code, _) = rc_net::pair::new_pair_code().unwrap();
    let code_hash = rc_net::pair::hash_pair_code(&code).unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &code_hash, NOW, NOW + 300_000).unwrap();
    let (token, token_hash) = rc_net::pair::new_token().unwrap();
    let outcome = rc_ledger::device::pair(
        root.ledger_mut(),
        &code_hash,
        &dev.device_id(),
        &dev.public_b64(),
        &token_hash,
        NOW + 1,
    )
    .unwrap();
    assert!(matches!(outcome, rc_ledger::device::PairOutcome::Paired));
    (dev, token)
}

fn one_commit(root: &mut Root) {
    let session = SessionId {
        session_key: "s-1".into(),
        device_id: "dev-1".into(),
        source_agent: "qoder".into(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
    };
    let body = "alpha";
    let turn = NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: compute_turn_uid("dev-1", "s-1", "human", body, 100),
        body: body.into(),
        body_fp: compute_body_fp(body),
        role: "human".into(),
        created_at_ms: 100,
    };
    insert_batch_committed(root.transcript_mut(), &session, &[turn], 100, "import").unwrap();
}

fn count_device(l: &rc_store::LedgerHandle) -> i64 {
    l.conn()
        .query_row("SELECT count(*) FROM device", [], |r| r.get(0))
        .unwrap()
}

fn capability_revoked(l: &rc_store::LedgerHandle, device_id: &str) -> bool {
    let revoked: Option<i64> = l
        .conn()
        .query_row(
            "SELECT revoked_at_ms FROM device_capability WHERE device_id=?1 AND capability='replicate'",
            [device_id],
            |r| r.get(0),
        )
        .unwrap();
    revoked.is_some()
}

#[test]
fn ledger_snapshot_replicates_tokens_capabilities_and_audits() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = Root::open(&dir.path().join("center")).unwrap();
    one_commit(&mut center);
    let (dev, _token) = pair_device(&mut center);
    rc_ledger::device::grant_capability(
        center.ledger_mut(),
        &dev.device_id(),
        rc_ledger::device::CAPABILITY_REPLICATE,
        NOW + 2,
    )
    .unwrap();
    let snapshot = export_ledger(&center).unwrap();
    assert_eq!(snapshot.commit_seq, 1);
    assert_eq!(snapshot.devices.len(), 1);
    assert_eq!(snapshot.capabilities.len(), 1);
    assert!(!snapshot.audits.is_empty());

    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let out = apply_ledger(backup.ledger_mut(), &snapshot, 500).unwrap();
    assert!(out.applied);
    assert_eq!(count_device(backup.ledger()), 1);
    assert!(!capability_revoked(backup.ledger(), &dev.device_id()));
    let status = ledger_apply_status(backup.ledger()).unwrap();
    assert_eq!(status.commit_seq, 1);
    assert_eq!(status.snapshot_sha256, snapshot.snapshot_sha256);

    let replay = apply_ledger(backup.ledger_mut(), &snapshot, 600).unwrap();
    assert!(!replay.applied, "同一 seq 同一 sha 必须跳过");

    // 同一 commit_seq 上撤销能力：内容变了，但 seq 没变；apply 必须照收。
    rc_ledger::device::revoke_capability(
        center.ledger_mut(),
        &dev.device_id(),
        rc_ledger::device::CAPABILITY_REPLICATE,
        NOW + 3,
    )
    .unwrap();
    let changed = export_ledger(&center).unwrap();
    assert_eq!(changed.commit_seq, snapshot.commit_seq);
    assert_ne!(changed.snapshot_sha256, snapshot.snapshot_sha256);
    let out = apply_ledger(backup.ledger_mut(), &changed, 700).unwrap();
    assert!(out.applied);
    assert!(capability_revoked(backup.ledger(), &dev.device_id()));
}

#[test]
fn a_tampered_ledger_snapshot_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let center = Root::open(&dir.path().join("center")).unwrap();
    let snapshot = export_ledger(&center).unwrap();
    let mut tampered = snapshot.clone();
    tampered.devices.push(hub::replicate::DeviceExport {
        device_id: "injected".into(),
        pubkey: "x".into(),
        token_hash: "y".into(),
        paired_at_ms: 1,
        revoked_at_ms: None,
    });
    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let error = apply_ledger(backup.ledger_mut(), &tampered, 500).unwrap_err();
    assert!(matches!(error, LedgerApplyError::BadSnapshot("snapshot_sha256")), "{error:?}");
    assert_eq!(count_device(backup.ledger()), 0);
}

#[test]
fn ledger_snapshot_carries_processing_watermarks_and_extract_states() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = Root::open(&dir.path().join("center")).unwrap();
    one_commit(&mut center);
    center
        .ledger()
        .conn()
        .execute(
            "INSERT INTO job_watermark(family, scope_key, cursor_at_ms, cursor_turn_uid,
                                       attempts, last_success_at_ms, halted_at_ms)
             VALUES('distill', '', 100, 't-1', 3, 90, NULL)",
            [],
        )
        .unwrap();
    center
        .ledger()
        .conn()
        .execute(
            "INSERT INTO extract_state(turn_uid, state, reason, at_ms)
             VALUES('t-1', 'extracted', NULL, 100)",
            [],
        )
        .unwrap();

    let snapshot = export_ledger(&center).unwrap();
    assert_eq!(snapshot.watermarks.len(), 1);
    assert_eq!(snapshot.extract_states.len(), 1);
    assert_eq!(snapshot.watermarks[0].cursor_at_ms, 100);
    assert_eq!(snapshot.extract_states[0].state, "extracted");

    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let outcome = apply_ledger(backup.ledger_mut(), &snapshot, 500).unwrap();
    assert!(outcome.applied);
    assert_eq!(outcome.watermarks, 1);
    assert_eq!(outcome.extract_states, 1);
    let (cursor, attempts): (i64, i64) = backup
        .ledger()
        .conn()
        .query_row(
            "SELECT cursor_at_ms, attempts FROM job_watermark
             WHERE family='distill' AND scope_key=''",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((cursor, attempts), (100, 3));
    let state: String = backup
        .ledger()
        .conn()
        .query_row(
            "SELECT state FROM extract_state WHERE turn_uid='t-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "extracted");
}

#[test]
fn a_backup_with_replicated_ledger_authenticates_an_old_device() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = Root::open(&dir.path().join("center")).unwrap();
    one_commit(&mut center);
    let (dev, token) = pair_device(&mut center);
    rc_ledger::device::grant_capability(
        center.ledger_mut(),
        &dev.device_id(),
        rc_ledger::device::CAPABILITY_REPLICATE,
        NOW + 2,
    )
    .unwrap();
    let snapshot = export_ledger(&center).unwrap();
    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    apply_ledger(backup.ledger_mut(), &snapshot, 500).unwrap();

    let hub_key = HubKey::load_or_create(&dir.path().join("backup.key")).unwrap();
    let mut state = HubState::new(
        backup,
        hub_key,
        HubConfig {
            bind: "127.0.0.1:7420".into(),
            endpoints: vec!["http://127.0.0.1:7420".into()],
            max_snapshot_bps: 4096,
        },
        &dir.path().join("backup"),
        Box::new(|| NOW),
    );
    // 直接签 v1 的 GET：备份 epoch=1，服务端仍兼容旧客户端。
    let nonce = challenge(&mut state, &dev, &token);
    let canonical = rc_net::protocol::request_canonical(
        "GET",
        "/v1/replicate/status",
        &rc_net::protocol::sha256_hex(b""),
        &dev.device_id(),
        &nonce,
    );
    let headers = vec![
        ("Authorization".into(), format!("Bearer {token}")),
        ("X-Recollect-Device".into(), dev.device_id()),
        ("X-Recollect-Nonce".into(), nonce),
        ("X-Recollect-Sig".into(), dev.sign_b64(canonical.as_bytes())),
    ];
    let resp = state.handle(&Request::new(
        "GET",
        "/v1/replicate/status",
        headers,
        Vec::new(),
    ));
    assert_eq!(resp.status, 200, "{}", String::from_utf8_lossy(&resp.body));
}

fn challenge(state: &mut HubState, dev: &DeviceKey, token: &str) -> String {
    let resp = state.handle(&Request::new(
        "POST",
        "/v1/challenge",
        vec![
            ("Authorization".into(), format!("Bearer {token}")),
            ("X-Recollect-Device".into(), dev.device_id()),
        ],
        Vec::new(),
    ));
    assert_eq!(resp.status, 200);
    let doc: Value = serde_json::from_slice(&resp.body).unwrap();
    doc["nonce"].as_str().unwrap().to_string()
}

#[test]
fn ledger_cli_export_apply_and_status_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let center_dir = dir.path().join("center");
    let mut center = Root::open(&center_dir).unwrap();
    one_commit(&mut center);
    let (dev, _) = pair_device(&mut center);
    rc_ledger::device::grant_capability(
        center.ledger_mut(),
        &dev.device_id(),
        rc_ledger::device::CAPABILITY_REPLICATE,
        NOW + 2,
    )
    .unwrap();
    drop(center);
    let binary = env!("CARGO_BIN_EXE_recollect-hub");
    let file = dir.path().join("ledger.json");
    let out = Command::new(binary)
        .args([
            "replicate",
            "export-ledger",
            "--data-dir",
            center_dir.to_str().unwrap(),
            "--output",
            file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["devices"], 1);
    assert_eq!(doc["capabilities"], 1);

    let backup_dir = dir.path().join("backup");
    let out = Command::new(binary)
        .args([
            "replicate",
            "apply-ledger",
            "--data-dir",
            backup_dir.to_str().unwrap(),
            "--input",
            file.to_str().unwrap(),
            "--now-ms",
            "500",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["applied"], true);

    let out = Command::new(binary)
        .args([
            "replicate",
            "ledger-status",
            "--data-dir",
            backup_dir.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["commit_seq"], 1);
}

// ── S1：抽取指纹随账本快照复制 ────────────────────────────────────────────────
//
// 两条：① 带指纹的快照 apply 之后备份里读得到；② 旧快照（没有这个键）既过 sha 校验，
// 又不许把备份自己已有的指纹抹成 NULL（`COALESCE(excluded, existing)` 的牙）。

#[test]
fn ledger_snapshot_carries_the_extraction_fingerprint_to_the_backup() {
    let dir = tempfile::tempdir().unwrap();
    let center = Root::open(&dir.path().join("center")).unwrap();
    let fp = "a".repeat(64);
    center.ledger().conn().execute(
        "INSERT INTO extract_state(turn_uid, state, reason, at_ms, extraction_fingerprint)
         VALUES('t-1', 'extracted', NULL, 100, ?1)",
        [fp.as_str()],
    ).unwrap();

    let snapshot = export_ledger(&center).unwrap();
    assert_eq!(snapshot.extract_states.len(), 1);
    assert_eq!(snapshot.extract_states[0].extraction_fingerprint.as_deref(), Some(fp.as_str()),
               "export 的 SELECT 没带新列");

    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let outcome = apply_ledger(backup.ledger_mut(), &snapshot, 500).unwrap();
    assert!(outcome.applied);
    let got: Option<String> = backup.ledger().conn().query_row(
        "SELECT extraction_fingerprint FROM extract_state WHERE turn_uid='t-1'",
        [], |r| r.get(0)).unwrap();
    assert_eq!(got.as_deref(), Some(fp.as_str()), "指纹没跟着快照到备份那一侧");
}

#[test]
fn a_legacy_snapshot_json_passes_the_sha_and_keeps_the_backups_own_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let center = Root::open(&dir.path().join("center")).unwrap();
    // 旧形状的写入口：不给新列 ⇒ 快照里这一格是 None。
    center.ledger().conn().execute(
        "INSERT INTO extract_state(turn_uid, state, reason, at_ms)
         VALUES('t-1', 'extracted', NULL, 100)",
        [],
    ).unwrap();

    let snapshot = export_ledger(&center).unwrap();
    let text = serde_json::to_string(&snapshot).unwrap();
    assert!(!text.contains("extraction_fingerprint"),
            "旧快照（全 None）的 JSON 里多出了这个键 ⇒ 升级前的 sha 会失配：{text}");
    // 真正走一次文本往返：`ledger_snapshot_sha256` 的输入元组形状没变，
    // 所以"没有这个键的旧 JSON"重算出来的 sha 必须还是快照里那一个。
    let old: hub::replicate::LedgerSnapshot = serde_json::from_str(&text).unwrap();
    assert!(old.extract_states.iter().all(|s| s.extraction_fingerprint.is_none()));

    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let mine = "b".repeat(64);
    backup.ledger().conn().execute(
        "INSERT INTO extract_state(turn_uid, state, reason, at_ms, extraction_fingerprint)
         VALUES('t-1', 'pending', NULL, 1, ?1)",
        [mine.as_str()],
    ).unwrap();
    // apply 内部会重算 sha 并与快照里那一个对拍：过不去就是 BadSnapshot，这里会直接红。
    let outcome = apply_ledger(backup.ledger_mut(), &old, 500).unwrap();
    assert!(outcome.applied, "无新键的旧快照必须仍能 apply");
    let (state, got): (String, Option<String>) = backup.ledger().conn().query_row(
        "SELECT state, extraction_fingerprint FROM extract_state WHERE turn_uid='t-1'",
        [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
    assert_eq!(state, "extracted", "快照的其余列照旧覆盖备份");
    assert_eq!(got.as_deref(), Some(mine.as_str()),
               "旧快照把备份自己的指纹抹成 NULL 了（COALESCE 没生效）");
}

// ── H7c2：审查轴水位随账本快照复制 ────────────────────────────────────────────
//
// 两条：① 空表 ⇒ 快照 JSON 里没有这个键、sha 与 H7c2 之前那条 8 元组公式逐字节一致
// （旧备份/旧快照仍过校验）；② 非空 ⇒ 行随快照到备份、同 seq 上改水位也会被 apply 识别。

fn put_mark(l: &rc_store::LedgerHandle, pass: &str, max_decision_id: i64) {
    rc_ledger::review_mark::put(
        l,
        &rc_ledger::review_mark::ReviewWatermark {
            pass_id: pass.into(),
            max_decision_id,
            pending_review: 2,
            quarantined: 1,
            applied: 4,
            updated_at_ms: 900,
        },
    )
    .unwrap();
}

/// H7c2 之前的 preimage：8 元组（不含 `review_watermarks`），在测试里独立拼一遍。
fn legacy_sha(snapshot: &hub::replicate::LedgerSnapshot) -> String {
    rc_net::protocol::sha256_hex(
        &serde_json::to_vec(&(
            snapshot.format_version,
            snapshot.commit_seq,
            snapshot.epoch,
            &snapshot.devices,
            &snapshot.capabilities,
            &snapshot.audits,
            &snapshot.watermarks,
            &snapshot.extract_states,
        ))
        .unwrap(),
    )
}

#[test]
fn an_empty_review_watermark_table_keeps_the_legacy_sha_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let mut center = Root::open(&dir.path().join("center")).unwrap();
    one_commit(&mut center);
    let snapshot = export_ledger(&center).unwrap();
    assert!(snapshot.review_watermarks.is_empty());

    let text = serde_json::to_string(&snapshot).unwrap();
    assert!(
        !text.contains("review_watermarks"),
        "空表不许出键（旧 sha 会失配）：{text}"
    );
    assert_eq!(
        snapshot.snapshot_sha256,
        legacy_sha(&snapshot),
        "空表时 sha 必须逐字节等于旧公式（8 元组）"
    );

    // 文本往返（模拟磁盘上的旧快照文件）：没有这个键也解析得出来，且仍过 apply 的校验。
    let old: hub::replicate::LedgerSnapshot = serde_json::from_str(&text).unwrap();
    assert!(old.review_watermarks.is_empty());
    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let outcome = apply_ledger(backup.ledger_mut(), &old, 500).unwrap();
    assert!(outcome.applied, "空表快照必须仍能 apply（旧 sha 兼容）");
    assert_eq!(outcome.review_watermarks, 0);
}

#[test]
fn review_watermarks_replicate_line_by_line_and_enter_the_sha() {
    let dir = tempfile::tempdir().unwrap();
    let center = Root::open(&dir.path().join("center")).unwrap();
    put_mark(center.ledger(), "fact", 7);
    put_mark(center.ledger(), "naming", 3);

    let snapshot = export_ledger(&center).unwrap();
    assert_eq!(snapshot.review_watermarks.len(), 2, "两个 pass 两行");
    assert_eq!(
        snapshot
            .review_watermarks
            .iter()
            .map(|w| (w.pass_id.as_str(), w.max_decision_id))
            .collect::<Vec<_>>(),
        vec![("fact", 7), ("naming", 3)],
        "导出按 pass_id 升序、字段逐字带上"
    );
    assert_eq!(snapshot.review_watermarks[0].updated_at_ms, 900, "updated_at_ms 也随快照走");
    assert!(
        serde_json::to_string(&snapshot)
            .unwrap()
            .contains("review_watermarks"),
        "非空必须出键"
    );
    assert_ne!(
        snapshot.snapshot_sha256,
        legacy_sha(&snapshot),
        "非空时该字段要进 preimage"
    );

    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    let outcome = apply_ledger(backup.ledger_mut(), &snapshot, 500).unwrap();
    assert!(outcome.applied);
    assert_eq!(outcome.review_watermarks, 2);
    assert_eq!(
        rc_ledger::review_mark::all(backup.ledger()).unwrap(),
        rc_ledger::review_mark::all(center.ledger()).unwrap(),
        "备份那一侧必须逐行相等（含三态与 updated_at_ms）"
    );

    // 同一 commit_seq 上改水位：内容变了、seq 没变 ⇒ sha 必须变、apply 必须照收。
    put_mark(center.ledger(), "fact", 9);
    let changed = export_ledger(&center).unwrap();
    assert_eq!(changed.commit_seq, snapshot.commit_seq);
    assert_ne!(changed.snapshot_sha256, snapshot.snapshot_sha256);
    // 备份自己的陈旧行（中心没有这个 pass）：整表替换必须把它带走。
    put_mark(backup.ledger(), "time", 99);
    let outcome = apply_ledger(backup.ledger_mut(), &changed, 600).unwrap();
    assert!(outcome.applied, "同 seq 的水位变化必须被识别，不是跳过");
    let rows = rc_ledger::review_mark::all(backup.ledger()).unwrap();
    assert_eq!(rows.len(), 2, "中心没有的 pass 行必须被删掉：{rows:?}");
    assert!(rows.iter().all(|w| w.pass_id != "time"), "整表替换不是逐行 upsert：{rows:?}");
    assert_eq!(rows[0].pass_id, "fact");
    assert_eq!(rows[0].max_decision_id, 9, "水位覆盖到备份那一侧");
}
