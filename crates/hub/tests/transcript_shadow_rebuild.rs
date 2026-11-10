//! W38a 刀 B 工单 4/5③：replicate（`apply_memory`）与 restore 两个既有 `rc_index::rebuild`
//! 调用点旁补 `rc_ledger::rebuild_turn_shadow`。刀 A 查明的影子缺口：这两条路径
//! 整替/落地转录层内容时若转录来自刀 A 之前的旧备份/旧导入，turns 在而影子空，
//! 检索面读数只剩 coverage 缺口。
//!
//! 这两条用例用**等价证明**钉接线时点（工单 5③ 的"或"支）：把影子行清空（模拟旧库），
//! 跑过该路径之后影子必须当场回来、FTS MATCH 当场命中——期间不跑任何额外运维命令。
//! 判据走 rusqlite 直查 `turn_shadow`：工单允许文件集里没有 hub/Cargo.toml，
//! hub→rc-search 这条新依赖边不由测试顺手开；FTS MATCH 命中 + 零缺口与
//! `rc_search::transcript::search_turns` 的可见半边同源（闸那半边在 rc-ledger/rc-search 的钉里）。
//!
//! 夹具纪律：会话名 / agent / principal / 正文全部自造合成词，不引用任何真实语料。

use rc_contract::{compute_body_fp, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch_committed;
use rc_net::key::HubKey;
use rc_net::snapshot::SnapshotState;
use rc_store::Root;
use std::path::PathBuf;
use std::process::{Command, Output};

/// 命中词（自造合成词）。
const MARKER: &str = "quokka";
const TURN_UID: &str = "1111111111111111111111111111111111111111111111111111111111111111";

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_recollect-hub"))
        .args(args)
        .output()
        .expect("hub binary")
}

fn seed_session_turn(root: &mut Root, body: &str, at: i64) {
    let session = SessionId {
        session_key: "ses-a".to_string(),
        device_id: "dev-a".to_string(),
        source_agent: "agent-alpha".to_string(),
        machine: None,
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: Some("p-one".to_string()),
        principal_name: None,
        principal_source: None,
    };
    let turn = NormalizedTurn {
        tier: TurnTier::ContentOnly,
        turn_uid: TURN_UID.to_string(),
        body: body.to_string(),
        body_fp: compute_body_fp(body),
        role: "human".to_string(),
        created_at_ms: at,
    };
    insert_batch_committed(root.transcript_mut(), &session, &[turn], at, "import").unwrap();
}

/// FTS 直查影子表（等价证明的判据面）。
fn shadow_hits(root: &Root, token: &str) -> Vec<String> {
    let mut st = root
        .transcript()
        .conn()
        .prepare("SELECT turn_uid FROM turn_shadow WHERE turn_shadow MATCH ?1")
        .unwrap();
    let rows = st.query_map([token], |r| r.get::<_, String>(0)).unwrap();
    rows.map(|r| r.unwrap()).collect()
}

/// (影子行数, 权威轮数)——零缺口即两者相等。
fn shadow_and_turn_counts(root: &Root) -> (i64, i64) {
    let conn = root.transcript().conn();
    let shadow: i64 =
        conn.query_row("SELECT count(*) FROM turn_shadow", [], |r| r.get(0)).unwrap();
    let turns: i64 = conn.query_row("SELECT count(*) FROM turns", [], |r| r.get(0)).unwrap();
    (shadow, turns)
}

// ── restore（restore.rs 的 rebuild 点） ─────────────────────────────────────

#[test]
fn restore_rebuilds_the_turn_shadow_so_old_backups_are_searchable_immediately() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = Root::open(&data).unwrap();
        seed_session_turn(&mut root, "restored text keeps the quokka marker", 1000);
    }
    let out = run(&[
        "backup",
        "--data-dir",
        data.to_str().unwrap(),
        "--now-ms",
        "5000",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "夹具备份必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = data.join("backups").join("backup-5000.json");
    assert!(manifest.is_file(), "夹具清单没落下：{}", manifest.display());

    // 旧备份模拟：刀 A 之前拍的备份没有影子行——把备份里的影子清空。
    {
        let conn =
            rusqlite::Connection::open(data.join("backups").join("transcript-5000.db")).unwrap();
        conn.execute("DELETE FROM turn_shadow", []).unwrap();
    }
    // 现场损毁：turns 与影子一起清空（恢复必须把两者都带回来）。
    {
        let root = Root::open(&data).unwrap();
        root.transcript().conn().execute("DELETE FROM turns", []).unwrap();
        root.transcript().conn().execute("DELETE FROM turn_shadow", []).unwrap();
    }

    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--confirm",
        "--data-dir",
        data.to_str().unwrap(),
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "确认之后恢复必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let root = Root::open(&data).unwrap();
    assert_eq!(
        shadow_and_turn_counts(&root),
        (1, 1),
        "恢复之后影子与权威面必须零缺口"
    );
    assert_eq!(
        shadow_hits(&root, MARKER),
        vec![TURN_UID.to_string()],
        "恢复之后必须能当场 FTS 命中导入的轮（不跑任何额外运维命令）"
    );
}

// ── replicate（replicate.rs 的 rebuild 点） ─────────────────────────────────

/// 中心夹具（照 `replicate_memory.rs` 的 `center_with_card`）：一卡 + 一份备份通道快照。
fn center_with_snapshot(dir: &std::path::Path) -> (PathBuf, String, u64) {
    std::fs::create_dir_all(dir).unwrap();
    let mut root = Root::open(dir).unwrap();
    use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
    put_card(
        root.memory(),
        &FactContent {
            kind: "note".into(),
            title: "快照卡".into(),
            body: "快照正文".into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: TURN_UID.into(), anchor_role: AnchorRole::Human }],
        100,
    )
    .unwrap();
    let key = HubKey::load_or_create(&dir.join("hub.key")).unwrap();
    let report = hub::snapshot::publish_channel(
        &mut root,
        &key,
        dir,
        1_000,
        hub::snapshot::SnapshotChannel::Backup,
    )
    .unwrap();
    let snaps = hub::snapshot::channel_dir(dir, hub::snapshot::SnapshotChannel::Backup);
    let state: SnapshotState =
        serde_json::from_slice(&std::fs::read(snaps.join("CURRENT")).unwrap()).unwrap();
    let file = snaps.join(rc_net::snapshot::snapshot_name(
        report.manifest.epoch,
        report.manifest.seq,
    ));
    (file, report.manifest.sha256, state.center_commit_seq)
}

#[test]
fn memory_apply_rebuilds_the_turn_shadow_too() {
    let dir = tempfile::tempdir().unwrap();
    let (snapshot, sha, commit_seq) = center_with_snapshot(&dir.path().join("center"));
    let copied = dir.path().join("memory-snapshot.db");
    std::fs::copy(&snapshot, &copied).unwrap();

    // 备份根：转录层有一轮（旧导入落地），影子是空的（刀 A 之前的库形）。
    let mut backup = Root::open(&dir.path().join("backup")).unwrap();
    seed_session_turn(&mut backup, "imported text keeps the quokka marker", 1000);
    backup.transcript().conn().execute("DELETE FROM turn_shadow", []).unwrap();
    assert!(
        shadow_hits(&backup, MARKER).is_empty(),
        "前置：影子必须真的是空的"
    );

    let out = hub::replicate::apply_memory(&mut backup, &copied, &sha, commit_seq, 500).unwrap();
    assert!(out.applied, "夹具 apply 必须真落地：{out:?}");
    assert_eq!(
        shadow_and_turn_counts(&backup),
        (1, 1),
        "apply_memory 之后影子必须零缺口（rebuild 时点接线正确）"
    );
    assert_eq!(
        shadow_hits(&backup, MARKER),
        vec![TURN_UID.to_string()],
        "apply_memory 之后必须能当场 FTS 命中"
    );
}
