//! V4 会话分片的读数：名字不泄露原串、同会话幂等、按 mtime 留最近 64 个。

use rc_store::shards;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

fn shard_name(session: &str) -> String {
    let mut h = Sha256::new();
    h.update(session.as_bytes());
    let hex: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    format!("session-{hex}.ids")
}

fn shard_files(data_dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(shards::dir(data_dir))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("session-") && n.ends_with(".ids"))
        .collect();
    names.sort();
    names
}

/// 判据 4⑭ 的正身：同会话写两次、同一句柄去重，文件名只含 sha256、内容一行一个句柄。
#[test]
fn recording_is_idempotent_and_the_name_does_not_leak_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let session = "sess-synthetic-do-not-leak-me";
    let first = vec!["card:one".to_string(), "card:two".to_string()];
    let r1 = shards::record(data, session, &first, 1_000).unwrap();
    assert_eq!((r1.shards, r1.evicted), (1, 0), "第一次写只该有一个分片");

    // 第二次故意换序并塞重复句柄：去重 + 升序之后，落盘字节必须与第一次相同。
    let twice = vec![
        "card:two".to_string(),
        "card:one".to_string(),
        "card:one".to_string(),
    ];
    let r2 = shards::record(data, session, &twice, 2_000).unwrap();
    assert_eq!((r2.shards, r2.evicted), (1, 0), "重复写不许长出新分片");

    let names = shard_files(data);
    assert_eq!(names, vec![shard_name(session)], "文件名必须是 session-<sha256>.ids");
    assert!(!names[0].contains(session), "文件名泄露了原会话串：{}", names[0]);

    let body = fs::read_to_string(shards::dir(data).join(&names[0])).unwrap();
    let lines: Vec<&str> = body.lines().collect();
    assert_eq!(lines, vec!["card:one", "card:two"], "一行一个句柄、升序去重：{body:?}");

    // 空会话是调用方错误，不是"写了个空名分片"。
    let err = shards::record(data, "   ", &first, 3_000).unwrap_err();
    assert!(err.to_string().contains("session key is empty"), "{err}");
}

/// 判据 4⑮ 的正身：100 个会话按 mtime 留最近 64 个，最旧的 36 个被删，
/// `inject/evicted` 的十进制计数就是 36。
#[test]
fn retention_keeps_the_newest_sixty_four_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path();
    let base = 1_700_000_000_000_i64;
    for i in 0..100_i64 {
        let session = format!("sess-{i:03}");
        let keys = vec![format!("card:{i}")];
        let now = base + i * 1_000;
        shards::record(data, &session, &keys, now).unwrap();
        // 显式把 mtime 盖成递增的时刻（保留排序只认文件 mtime；不靠调用顺序）。
        let path = shards::dir(data).join(shard_name(&session));
        let f = fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_modified(UNIX_EPOCH + Duration::from_millis(now as u64)).unwrap();
    }

    let r = shards::reading(data);
    assert_eq!(r.shards, shards::RETAIN_SESSIONS, "上限必须是 64");
    assert_eq!(r.evicted, 36, "100 - 64 = 36 次淘汰");
    println!(
        "retention reading: sessions_written=100 shards={} evicted={}",
        r.shards, r.evicted
    );

    let present = shard_files(data);
    for i in 0..36 {
        let gone = shard_name(&format!("sess-{i:03}"));
        assert!(!present.contains(&gone), "最旧的 sess-{i:03} 没被淘汰");
    }
    for i in 36..100 {
        let kept = shard_name(&format!("sess-{i:03}"));
        assert!(present.contains(&kept), "sess-{i:03} 该被保留");
    }
    assert_eq!(
        fs::read_to_string(shards::dir(data).join(shards::EVICTED_FILE)).unwrap(),
        "36",
        "淘汰计数是十进制文件，不是 ledger 计数"
    );
}

#[test]
fn reading_an_absent_directory_is_zero() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("never-created");
    let r = shards::reading(&missing);
    assert_eq!((r.shards, r.evicted), (0, 0), "目录不在就是零读数，不是失败");
    assert!(!shards::dir(&missing).exists(), "reading 不许顺手建目录");
}
