use serde_json::Value;
use std::path::Path;
use std::process::Command;

use rc_contract::{compute_body_fp, compute_turn_uid, NormalizedTurn, SessionId, TurnTier};
use rc_ledger::transcript::insert_batch_committed;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn publish(data: &Path) -> Value {
    publish_with(data, &[])
}

/// `extra` 是追加在 `publish` 之后的参数（`--channel ...` 之类）。
fn publish_with(data: &Path, extra: &[&str]) -> Value {
    let out = Command::new(bin())
        .arg("snapshot")
        .arg("publish")
        .args(extra)
        .arg("--data-dir")
        .arg(data)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        !text.contains(data.to_string_lossy().as_ref()),
        "快照输出带了路径：{text}"
    );
    serde_json::from_str(text.trim()).unwrap()
}

/// 某个快照目录下的一代文件（目录由通道决定，调用方给）。
fn snapshot_files_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("snapshot-") && n.ends_with(".db"))
        .collect();
    names.sort();
    names
}

fn snapshot_files(data: &Path) -> Vec<String> {
    snapshot_files_in(&data.join("snapshots"))
}

#[test]
fn publish_prints_a_manifest_and_writes_one_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data-canary-xyz");
    let doc = publish(&data);
    assert_eq!(doc["published"], true);
    assert_eq!(doc["channel"], "agent", "默认发布必须走记忆通道");
    assert!(doc["epoch"].as_u64().unwrap() > 0);
    assert_eq!(doc["seq"], 1);
    assert!(doc["bytes"].as_u64().unwrap() > 0);
    assert_eq!(doc["dead_rows"], 0);
    assert_eq!(doc["orphan_edges"], 0);
    assert_eq!(doc["orphan_derived"], 0);

    let files = snapshot_files(&data);
    assert_eq!(files.len(), 1, "{files:?}");
    let current: Value =
        serde_json::from_slice(&std::fs::read(data.join("snapshots").join("CURRENT")).unwrap())
            .unwrap();
    let name = current["file"].as_str().unwrap();
    assert_eq!(name, files[0]);
    assert!(
        !name.contains('/') && !name.contains('\\'),
        "状态里存了路径而不是文件名：{name}"
    );
    assert!(data.join("hub.key").exists(), "发布没有生成中心私钥");
    assert!(!data.join("snapshots").join("CURRENT.bak").exists());
}

#[test]
fn snapshot_state_carries_the_center_commit_seq() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let mut root = rc_store::Root::open(&data).unwrap();
        let body = "alpha";
        let turn = NormalizedTurn {
            tier: TurnTier::ContentOnly,
            turn_uid: compute_turn_uid("dev-1", "s-1", "human", body, 100),
            body: body.into(),
            body_fp: compute_body_fp(body),
            role: "human".into(),
            created_at_ms: 100,
        };
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
        insert_batch_committed(root.transcript_mut(), &session, &[turn], 100, "import").unwrap();
    }
    let _ = publish(&data);
    let current: Value =
        serde_json::from_slice(&std::fs::read(data.join("snapshots").join("CURRENT")).unwrap())
            .unwrap();
    assert_eq!(current["center_commit_seq"], 1);
}

#[test]
fn a_second_publish_of_unchanged_content_is_not_published() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let first = publish(&data);
    let second = publish(&data);
    assert_eq!(second["published"], false);
    assert_eq!(second["epoch"], first["epoch"]);
    assert_eq!(second["seq"], first["seq"]);
    assert_eq!(second["bytes"], first["bytes"]);
    assert_eq!(snapshot_files(&data).len(), 1);

    let leftovers: Vec<String> = std::fs::read_dir(data.join("snapshots"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".snapshot-tmp-"))
        .collect();
    assert!(leftovers.is_empty(), "没发布的临时文件留下了：{leftovers:?}");
    // 内容没变 ⇒ `publish` 在 `save_state` 之前就返回了，指针根本没被换：
    // `CURRENT` 还是第一份，`.bak` 不该出现（`.bak` 的回退行为归 T3-A 的状态用例管）。
    let current: Value =
        serde_json::from_slice(&std::fs::read(data.join("snapshots").join("CURRENT")).unwrap())
            .unwrap();
    // 偏离（登记在案）：计划字面写的是 `current["seq"]`，但 `CURRENT` 的真实形状是
    // `{"manifest":{...,"seq":N,...},"digest":...,"file":...}`（`rc_net::snapshot::SnapshotState`
    // 的字段就是 manifest/digest/file；计划自己的 `snapshot_http.rs` 字面块也是这么构造的）。
    // `current["seq"]` 读到的是 null，于是这条断言恒左 null 右 1 ⇒ 红的不是实现而是那一个下标。
    // 断言的原意（"指针还指着第一份的 seq"）一字未动，只把下标指到真正住 seq 的那一层。
    assert_eq!(current["manifest"]["seq"], first["seq"]);
    let files = snapshot_files(&data);
    assert_eq!(files.len(), 1);
    assert_eq!(current["file"].as_str().unwrap(), files[0]);
    assert!(!data.join("snapshots").join("CURRENT.bak").exists());
}

/// H7b：通道由 `--channel` 选，落到目录上。
///
/// 判据：默认发布写 `snapshots/`（记忆通道，回执 `channel=agent`）且不碰备份目录；
/// `--channel backup` 写 `snapshots-backup/`，两条目录各自有自己的 `CURRENT`；
/// 非法通道名是**用法错误**（退 3），也不许顺手发布一条通道。
#[test]
fn publish_channel_selects_the_directory_and_rejects_unknown_names() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");

    let agent = publish(&data);
    assert_eq!(agent["channel"], "agent");
    let agent_dir = data.join("snapshots");
    assert_eq!(snapshot_files_in(&agent_dir).len(), 1);
    assert!(agent_dir.join("CURRENT").exists());
    assert!(
        !data.join("snapshots-backup").exists(),
        "默认发布写了备份通道目录"
    );

    let backup = publish_with(&data, &["--channel", "backup"]);
    assert_eq!(backup["channel"], "backup");
    let backup_dir = data.join("snapshots-backup");
    assert_eq!(snapshot_files_in(&backup_dir).len(), 1);
    assert!(backup_dir.join("CURRENT").exists());
    // 两条通道的清单各住各的目录：备份发布不动记忆通道的指针与文件。
    assert_eq!(snapshot_files_in(&agent_dir).len(), 1);
    let agent_current: Value =
        serde_json::from_slice(&std::fs::read(agent_dir.join("CURRENT")).unwrap()).unwrap();
    assert_eq!(agent_current["manifest"]["seq"], agent["seq"]);

    let out = Command::new(bin())
        .args(["snapshot", "publish", "--channel", "weekly", "--data-dir"])
        .arg(&data)
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(3),
        "非法通道名必须是用法错误（退 3）：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.stdout.is_empty(), "用法错误不许回执一份发布报告");
    // 拒绝发生在前：两条通道都没有多出一代。
    assert_eq!(snapshot_files_in(&agent_dir).len(), 1);
    assert_eq!(snapshot_files_in(&backup_dir).len(), 1);
}

#[test]
fn a_retired_entity_is_counted_and_missing_from_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = rc_store::Root::open(&data).unwrap();
        let conn = root.memory().conn();
        conn.execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind, scope_key, status, profile_text, created_at_ms, updated_at_ms, norm_version)
             VALUES ('e-dead','concept','dead','unknown',NULL,'global','','active',NULL,1,1,1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO retirement(mem_table, mem_id, reason, created_at_ms, resolved_at_ms)
             VALUES ('anchor','e-dead','retired',2,NULL)",
            [],
        )
        .unwrap();
    }
    let doc = publish(&data);
    assert_eq!(doc["dead_rows"], 1);
    assert_eq!(doc["seq"], 1);

    let file = data.join("snapshots").join(&snapshot_files(&data)[0]);
    let snap = rusqlite::Connection::open(&file).unwrap();
    let left: i64 = snap
        .query_row("SELECT count(*) FROM anchor WHERE id='e-dead'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 0, "死行进了快照");
    let kept: i64 = snap
        .query_row(
            "SELECT count(*) FROM retirement WHERE mem_id='e-dead'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(kept, 1, "判定族被连带删了");
}
