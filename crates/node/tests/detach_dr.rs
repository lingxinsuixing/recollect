//! B4a：5 天自升、两段自备份与保留锚（判据 3⑩–3⑬）。
//!
//! 夹具照派发档 §三：`Root::open` 造转录 + 记忆；把记忆拷成 `replica/snapshot-1-1.db`；
//! 写一份中心签名清单原文 `replica/manifest-1-1.json`；`SyncState` 的 `current_manifest`
//! 指向它。自备份的 `st` 直接构造，不经过 `sync`。
//!
//! **下行段可验签，本机增量段只能自证**：这里断言两段都在、下行的 sha 与副本一致、
//! 中心清单逐字节搬进备份、本机段每份 sha 与文件一致。

use node::dr::{backup_once, last_backup_ms, rotate};
use node::offline::{observe_failure, DETACH_GATE_MS, OFFLINE_GATE_MS};
use node::state::{self, CurrentManifest, SelfDistill, SyncState};
use node::sync::SyncError;
use rc_contract::{compute_body_fp, compute_turn_uid};
use rc_store::{snapshot, Root};
use std::path::{Path, PathBuf};

const DEV: &str = "dev-detach";
const SESS: &str = "sess-detach";
const T0: i64 = 1_700_000_000_000;
const BASE_MS: i64 = 10_000_000;

fn synth_turn(t: &rc_store::TranscriptHandle, uid: &str, body: &str, at: i64) {
    // 形状照 `offline_cycle.rs` 的 `synth_turn`。
    t.conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
             VALUES(?1, ?2, ?3, 'human', ?4, ?5, 1, NULL, ?6, ?6)",
            (uid, SESS, DEV, body, compute_body_fp(body).as_str(), at),
        )
        .unwrap();
    t.conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine,
                                  project_hint, last_saved_at_ms)
             VALUES(?1, ?2, 'agent', NULL, NULL, ?3)
             ON CONFLICT(session_key, device_id)
             DO UPDATE SET last_saved_at_ms=excluded.last_saved_at_ms",
            (SESS, DEV, at),
        )
        .unwrap();
}

struct Fixture {
    _dir: tempfile::TempDir,
    data: PathBuf,
    st: SyncState,
    replica_sha: String,
}

/// 一份"同步过一代、攒了本机增量"的节点目录。
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("replica")).unwrap();

    let (first_uid, second_uid) = (
        compute_turn_uid(DEV, SESS, "human", "the first local turn", 1_000),
        compute_turn_uid(DEV, SESS, "human", "the second local turn", 4_000),
    );
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), &first_uid, "the first local turn", 1_000);
        synth_turn(root.transcript(), &second_uid, "the second local turn", 4_000);
        // WAL 的提交在最后一个连接关闭时落回主文件；先关再拷，拷贝才是完整的。
    }
    let memory = data.join("memory.db");
    let replica = data.join("replica/snapshot-1-1.db");
    std::fs::copy(&memory, &replica).unwrap();
    // 暂存层也在：自备份必须带上 pending.db（WAL 下用 VACUUM INTO，不是裸拷）。
    std::fs::copy(&memory, data.join("pending.db")).unwrap();
    let replica_sha = snapshot::file_sha256(&replica).unwrap();

    // 中心签名清单的原文。签名内容对自备份无关：`dr` 只按字节搬。
    let center = serde_json::json!({
        "api_version": 1,
        "epoch": 1u64,
        "seq": 1u64,
        "sha256": replica_sha.clone(),
        "bytes": std::fs::metadata(&replica).unwrap().len(),
        "schema_version": rc_store::ddl::MEMORY_VERSION,
        "norm_version": rc_store::capability::NORM_VERSION,
        "created_at_ms": 1_500,
        "signature": "arbitrary-signature-for-backup-test",
        "endpoint": "/v1/snapshot/memory",
        "max_chunk_bytes": 1024,
    });
    std::fs::write(
        data.join("replica/manifest-1-1.json"),
        serde_json::to_vec_pretty(&center).unwrap(),
    )
    .unwrap();

    let st = SyncState {
        current_manifest: CurrentManifest {
            epoch: 1,
            seq: 1,
            sha256: replica_sha.clone(),
            created_at_ms: 1_500,
        },
        self_distill: SelfDistill::DetachedDistilling,
        ..SyncState::default()
    };
    // `backup_once` 要求状态文件在场：它会把这份文件原样带进备份。
    state::save(&state::state_path(&data), &st).unwrap();

    Fixture { _dir: dir, data, st, replica_sha }
}

fn backup_dir(data: &Path, now_ms: i64) -> PathBuf {
    data.join("dr").join(format!("backup-{now_ms}"))
}

fn gate(now_ms: i64, auto: bool) -> SelfDistill {
    let mut st = SyncState::default();
    observe_failure(&mut st, SyncError::EndpointUnreachable, T0, 0, auto);
    observe_failure(&mut st, SyncError::EndpointUnreachable, now_ms, 0, auto)
}

#[test]
fn five_days_switch_to_detached() {
    // 一天到五天之间仍是一天的档；五天整（0 毫秒的差）才自升。
    assert_eq!(
        gate(T0 + OFFLINE_GATE_MS, true),
        SelfDistill::OfflineDistilling,
        "满一天、未满五天 ⇒ offline_distilling"
    );
    assert_eq!(
        gate(T0 + DETACH_GATE_MS - 1, true),
        SelfDistill::OfflineDistilling,
        "差一毫秒不进 detached"
    );
    assert_eq!(
        gate(T0 + DETACH_GATE_MS, true),
        SelfDistill::DetachedDistilling,
        "满五天 ⇒ detached_distilling"
    );

    // `auto_unlock=false` 仍然一档都不开：两档都是 `offline_waiting`。
    assert_eq!(gate(T0 + DETACH_GATE_MS - 1, false), SelfDistill::OfflineWaiting);
    assert_eq!(gate(T0 + DETACH_GATE_MS, false), SelfDistill::OfflineWaiting);
}

#[test]
fn a_detached_node_writes_a_two_segment_backup() {
    let fx = fixture();
    let now = BASE_MS;
    let report = backup_once(&fx.data, &fx.st, now).expect("有副本、有状态 ⇒ 自备份必须成功");
    let dir = backup_dir(&fx.data, now);
    assert_eq!(report.dir, dir);
    assert!(report.bytes > 0, "落盘字节数必须是个真读数：{}", report.bytes);
    assert_eq!(report.local_turn_highwater_ms, 4_000, "高水位取转录层最大到达钟");

    let doc: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();

    // 下行段：中心清单的签名字段 + 指向原文的文件名。
    let down = &doc["downlink"];
    assert_eq!(down["epoch"], 1);
    assert_eq!(down["seq"], 1);
    assert_eq!(down["sha256"], fx.replica_sha);
    assert_eq!(down["created_at_ms"], 1_500);
    assert_eq!(down["signed_manifest"], "center-manifest.json");
    assert_eq!(
        snapshot::file_sha256(&dir.join("replica.db")).unwrap(),
        fx.replica_sha,
        "下行段的副本必须与清单 sha 一致（普通拷贝，不加不改）"
    );

    // 中心签名清单原文逐字节搬进备份。
    assert_eq!(
        std::fs::read(dir.join("center-manifest.json")).unwrap(),
        std::fs::read(fx.data.join("replica/manifest-1-1.json")).unwrap(),
        "center-manifest.json 必须与源逐字节相同"
    );

    // 本机增量段：每份文件的 sha 与文件一致；高水位已写。
    let local = &doc["local"];
    assert_eq!(local["local_turn_highwater_ms"], 4_000);
    assert_eq!(local["created_at_ms"], now);
    let files = local["files"].as_object().expect("本机段必须带 files 表");
    for name in ["transcript.db", "pending.db", "node-state.json"] {
        assert!(files.contains_key(name), "本机段缺 {name}：{files:?}");
    }
    for (name, sha) in files {
        let got = snapshot::file_sha256(&dir.join(name)).unwrap();
        assert_eq!(sha.as_str().unwrap(), got, "{name} 的 sha 与文件不一致");
    }
}

#[test]
fn without_confirmation_backups_are_never_deleted() {
    let fx = fixture();
    for i in 0..8 {
        let now = BASE_MS + i * 1_000;
        backup_once(&fx.data, &fx.st, now).expect("自备份必须成功");
        assert_eq!(
            rotate(&fx.data, 0, 5).unwrap(),
            0,
            "中心未确认（0）时 rotate 一份都不许删"
        );
    }
    for i in 0..8 {
        assert!(backup_dir(&fx.data, BASE_MS + i * 1_000).is_dir(), "第 {} 份被删了", i + 1);
    }
    assert_eq!(last_backup_ms(&fx.data), Some(BASE_MS + 7_000));
}

#[test]
fn confirmation_allows_pruning_only_old_copies() {
    let fx = fixture();
    for i in 0..4 {
        backup_once(&fx.data, &fx.st, BASE_MS + i * 1_000).unwrap();
    }
    // 确认时刻设在第 4 次之后：第 4 份本身仍早于确认。
    let confirmed = BASE_MS + 3_000 + 1;
    assert_eq!(rotate(&fx.data, confirmed, 5).unwrap(), 0, "不足 5 份时一份都不删");

    let mut last = 0;
    for i in 4..9 {
        backup_once(&fx.data, &fx.st, BASE_MS + i * 1_000).unwrap();
        last = rotate(&fx.data, confirmed, 5).unwrap();
    }
    assert_eq!(last, 1, "最后一次只该删掉刚超窗的那一份");
    assert_eq!(rotate(&fx.data, confirmed, 5).unwrap(), 0, "再轮转一次不该再删");
    for i in 0..4 {
        assert!(
            !backup_dir(&fx.data, BASE_MS + i * 1_000).exists(),
            "早于确认且超窗的第 {} 份必须删掉",
            i + 1
        );
    }
    for i in 4..9 {
        assert!(
            backup_dir(&fx.data, BASE_MS + i * 1_000).is_dir(),
            "确认后的第 {} 份一份都不许删",
            i + 1
        );
    }
}
