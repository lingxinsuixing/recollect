use rc_net::error::NetError;
use rc_net::key::{verify_b64, HubKey};
use rc_net::protocol::{manifest_canonical, sha256_hex};
use rc_net::snapshot::{
    load_state, new_epoch, publish, retain, save_state, snapshot_name, Manifest, PublishOutcome,
    SnapshotBuild, SnapshotState, CURRENT_STATE,
};
use std::path::Path;

fn key(dir: &Path) -> HubKey {
    HubKey::load_or_create(&dir.join("hub.key")).unwrap()
}

fn temp_with(dir: &Path, name: &str, bytes: &[u8]) -> (std::path::PathBuf, String) {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    (p, sha256_hex(bytes))
}

fn build<'a>(temp: &'a Path, sha: &'a str, digest: &'a str, now_ms: i64) -> SnapshotBuild<'a> {
    SnapshotBuild {
        temp_db: temp,
        sha256: sha,
        digest,
        schema_version: 6,
        norm_version: 1,
        now_ms,
        center_commit_seq: 0,
    }
}

fn fixture_state(epoch: u64, seq: u64) -> SnapshotState {
    SnapshotState {
        manifest: Manifest {
            epoch,
            seq,
            sha256: "ab".repeat(32),
            schema_version: 6,
            norm_version: 1,
            bytes: 10,
            created_at_ms: 1,
            signature: "sig".into(),
        },
        digest: "d".into(),
        file: snapshot_name(epoch, seq),
        center_commit_seq: 0,
    }
}

#[test]
fn publish_names_the_file_and_signs_the_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let key = key(dir.path());
    let (temp, sha) = temp_with(dir.path(), ".tmp-a.db", b"snapshot-bytes");
    let out = publish(dir.path(), build(&temp, &sha, "d1", 42), &key, 3).unwrap();
    let PublishOutcome::Published(m) = out else {
        panic!("first publish must be fresh");
    };
    assert_eq!(m.seq, 1);
    assert_eq!(m.bytes, 14);
    assert_eq!(m.created_at_ms, 42);
    assert!(verify_b64(
        &key.public_b64(),
        manifest_canonical(m.epoch, 1, &sha, 6, 1).as_bytes(),
        &m.signature
    ));
    assert!(!temp.exists(), "临时名必须被改名，不许留第二份");
    assert!(dir.path().join(snapshot_name(m.epoch, 1)).exists());
    let state = load_state(dir.path()).unwrap().unwrap();
    assert_eq!(state.manifest, m);
    assert_eq!(state.digest, "d1");
    assert_eq!(state.file, snapshot_name(m.epoch, 1));
}

#[test]
fn same_digest_is_unchanged_and_advances_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let key = key(dir.path());
    let (t1, s1) = temp_with(dir.path(), ".tmp-a.db", b"first");
    let PublishOutcome::Published(first) = publish(dir.path(), build(&t1, &s1, "d1", 1), &key, 3)
        .unwrap()
    else {
        panic!()
    };
    let (t2, s2) = temp_with(dir.path(), ".tmp-b.db", b"second-and-different");
    let out = publish(dir.path(), build(&t2, &s2, "d1", 2), &key, 3).unwrap();
    let PublishOutcome::Unchanged(same) = out else {
        panic!("同一摘要必须是 unchanged");
    };
    assert_eq!((same.epoch, same.seq), (first.epoch, 1));
    assert_eq!(same.sha256, s1, "unchanged 返回的是旧清单，不是新文件");
    assert!(!t2.exists(), "unchanged 必须把新产出的临时文件删掉");
    let state = load_state(dir.path()).unwrap().unwrap();
    assert_eq!(state.manifest.sha256, s1);
    let files = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("snapshot-"))
        .count();
    assert_eq!(files, 1, "unchanged 不该留下第二份字节");
}

#[test]
fn changed_digest_advances_seq_and_keeps_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let key = key(dir.path());
    let (t1, s1) = temp_with(dir.path(), ".tmp-a.db", b"first");
    let PublishOutcome::Published(first) =
        publish(dir.path(), build(&t1, &s1, "d1", 1), &key, 3).unwrap()
    else {
        panic!()
    };
    let (t2, s2) = temp_with(dir.path(), ".tmp-b.db", b"second");
    let PublishOutcome::Published(second) =
        publish(dir.path(), build(&t2, &s2, "d2", 2), &key, 3).unwrap()
    else {
        panic!("内容变了必须发新快照")
    };
    assert_eq!(second.epoch, first.epoch, "同一中心不该换 epoch");
    assert_eq!(second.seq, first.seq + 1);
    assert_eq!(second.sha256, s2);
}

#[test]
fn retain_keeps_the_newest_and_never_the_current() {
    let dir = tempfile::tempdir().unwrap();
    for seq in 1..=5u64 {
        std::fs::write(dir.path().join(snapshot_name(7, seq)), b"x").unwrap();
    }
    // 当前指针指向**最旧**那一份：它必须活下来，因此最后剩下 4 份。
    let current = snapshot_name(7, 1);
    let removed = retain(dir.path(), 3, &current).unwrap();
    assert_eq!(removed, vec![snapshot_name(7, 2)]);
    for seq in [1u64, 3, 4, 5] {
        assert!(dir.path().join(snapshot_name(7, seq)).exists(), "seq {seq} 被误删");
    }
    // 当前指针在最新三份里时，只留三份。
    let removed = retain(dir.path(), 3, &snapshot_name(7, 5)).unwrap();
    assert_eq!(removed, vec![snapshot_name(7, 1)]);
}

#[test]
fn missing_current_falls_back_to_the_backup_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let first = fixture_state(7, 1);
    let second = fixture_state(7, 2);
    save_state(dir.path(), &first).unwrap();
    save_state(dir.path(), &second).unwrap();
    assert_eq!(load_state(dir.path()).unwrap(), Some(second));
    // 模拟"旧指针已挪走、新指针还没改名"的那一瞬。
    std::fs::rename(
        dir.path().join(CURRENT_STATE),
        dir.path().join("CURRENT.gone"),
    )
    .unwrap();
    assert_eq!(
        load_state(dir.path()).unwrap(),
        Some(first),
        "坏在两步之间时必须读回旧指针，而不是读成没有快照"
    );
}

#[test]
fn state_file_holds_a_file_name_not_a_path() {
    let dir = tempfile::tempdir().unwrap();
    let key = key(dir.path());
    let (temp, sha) = temp_with(dir.path(), ".tmp-a.db", b"snapshot-bytes");
    publish(dir.path(), build(&temp, &sha, "d1", 42), &key, 3).unwrap();
    let text = std::fs::read(dir.path().join(CURRENT_STATE)).unwrap();
    let text = String::from_utf8(text).unwrap();
    assert!(
        !text.contains('/') && !text.contains('\\'),
        "CURRENT 里出现了路径分隔符：{text}"
    );
    assert!(text.contains("snapshot-"), "文件名没进状态：{text}");
}

#[test]
fn new_epoch_is_nonzero_and_differs_between_calls() {
    let a = new_epoch().unwrap();
    let b = new_epoch().unwrap();
    assert_ne!(a, 0);
    assert_ne!(b, 0);
    assert_ne!(a, b, "两次随机撞车（2^-63）；再跑一次，不许放宽这条");
}

#[test]
fn publish_refuses_a_missing_temp_file_and_leaves_no_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let key = key(dir.path());
    let missing = dir.path().join("absent.db");
    let err = publish(dir.path(), build(&missing, "00", "d1", 1), &key, 3).unwrap_err();
    assert!(matches!(err, NetError::Io(_)), "{err:?}");
    assert!(load_state(dir.path()).unwrap().is_none(), "失败不该写出指针");
}
