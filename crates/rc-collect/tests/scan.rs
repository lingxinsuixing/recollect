//! files 源扫描与采集锁的行为测试。

use std::path::Path;

use rc_collect::config::{Source, SourceKind};
use rc_collect::lock::try_lock;
use rc_collect::scan::{head_sha256, path_key, scan_files, DueFile};
use rc_collect::state::{FileCursor, StateDoc};

fn src(root: &Path) -> Source {
    Source {
        name: "s".into(),
        kind: SourceKind::Files,
        root: Some(root.into()),
        glob: Some("*.jsonl".into()),
        db: None,
        cursor_sql: None,
        converter: vec!["true".into()],
        device_id: "dev".into(),
        source_agent: "a".into(),
        project: "session".into(),
    }
}

fn mtime_ms(p: &Path) -> i64 {
    std::fs::metadata(p)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn quiet_gate_skips_active_file_and_accepts_idle() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.jsonl");
    std::fs::write(&f, "{\"x\":1}\n").unwrap();
    let mt = mtime_ms(&f);
    let st = StateDoc::default();
    // 活跃：now 与 mtime 差 < quiet_ms
    assert!(scan_files(&src(d.path()), &st, 600_000, mt + 60_000).is_empty());
    // 静默：差 ≥ quiet_ms
    let due: Vec<DueFile> = scan_files(&src(d.path()), &st, 600_000, mt + 600_000);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].offset, 0);
    assert_eq!(due[0].size, 8);
}

#[test]
fn unchanged_file_is_not_due_and_head_rewrite_resets_offset() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.jsonl");
    std::fs::write(&f, "AAAA\n").unwrap();
    let mt = mtime_ms(&f);
    let mut st = StateDoc::default();
    st.files.insert(
        path_key(&f),
        FileCursor {
            mtime_ms: mt,
            size: 5,
            offset: 5,
            last_turns: 0,
            head_sha256: head_sha256(&f).unwrap(),
        },
    );
    assert!(scan_files(&src(d.path()), &st, 600_000, mt + 600_000).is_empty());

    // 同长重写：头哈希变 ⇒ offset 归零
    std::fs::write(&f, "BBBB\n").unwrap();
    let mt2 = mtime_ms(&f);
    let due = scan_files(&src(d.path()), &st, 600_000, mt2 + 600_000);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].offset, 0);
    assert_eq!(due[0].size, 5);
}

#[test]
fn truncated_file_resets_offset_to_zero() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("a.jsonl");
    std::fs::write(&f, "xy\n").unwrap();
    let mt = mtime_ms(&f);
    let mut st = StateDoc::default();
    st.files.insert(
        path_key(&f),
        FileCursor {
            mtime_ms: mt - 5_000,
            size: 100,
            offset: 99,
            last_turns: 3,
            head_sha256: "old".into(),
        },
    );
    let due = scan_files(&src(d.path()), &st, 600_000, mt + 600_000);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].offset, 0);
    assert_eq!(due[0].last_turns, 3);
}

#[test]
fn only_matching_files_are_returned_newest_first() {
    let d = tempfile::tempdir().unwrap();
    std::fs::write(d.path().join("a.jsonl"), "1\n").unwrap();
    std::fs::write(d.path().join("b.jsonl"), "2\n").unwrap();
    std::fs::write(d.path().join("skip.txt"), "3\n").unwrap();
    std::fs::create_dir(d.path().join("sub")).unwrap();
    std::fs::write(d.path().join("sub").join("c.jsonl"), "4\n").unwrap();
    let now = mtime_ms(&d.path().join("b.jsonl")) + 600_000;
    let due = scan_files(&src(d.path()), &StateDoc::default(), 600_000, now);
    assert_eq!(due.len(), 2, "single-layer glob must not descend");
    assert!(due.windows(2).all(|w| w[0].mtime_ms >= w[1].mtime_ms));
    assert!(due.iter().all(|f| f.path.extension().unwrap() == "jsonl"));
}

#[test]
fn head_hash_covers_first_4kb_only() {
    let d = tempfile::tempdir().unwrap();
    let f = d.path().join("big.jsonl");
    let mut body = vec![b'a'; 4096];
    body.extend_from_slice(b"tail-one");
    std::fs::write(&f, &body).unwrap();
    let h1 = head_sha256(&f).unwrap();
    body.truncate(4096);
    body.extend_from_slice(b"tail-two");
    std::fs::write(&f, &body).unwrap();
    assert_eq!(
        head_sha256(&f).unwrap(),
        h1,
        "tail must be outside the head"
    );
}

#[test]
fn lock_is_exclusive_and_releases_on_drop() {
    let d = tempfile::tempdir().unwrap();
    let first = try_lock(d.path()).unwrap();
    assert!(first.is_some());
    assert!(try_lock(d.path()).unwrap().is_none(), "live holder wins");
    drop(first);
    let again = try_lock(d.path()).unwrap();
    assert!(again.is_some(), "lock dir must be gone after drop");
}

#[test]
fn stale_lock_with_dead_pid_is_cleaned() {
    let d = tempfile::tempdir().unwrap();
    let lock_dir = d.path().join("collect.lock");
    std::fs::create_dir(&lock_dir).unwrap();
    // 远超常规 PID 范围，几乎所有系统上都已死。
    std::fs::write(lock_dir.join("pid"), "4000000").unwrap();
    let got = try_lock(d.path()).unwrap();
    assert!(got.is_some(), "dead holder must not block collection");
}

#[test]
fn lock_without_pid_file_is_cleaned() {
    let d = tempfile::tempdir().unwrap();
    std::fs::create_dir(d.path().join("collect.lock")).unwrap();
    let got = try_lock(d.path()).unwrap();
    assert!(got.is_some(), "half-created lock must be reclaimable");
}
