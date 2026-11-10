// crates/hub/tests/backup_cli.rs
//! T5-B / B5a：每日备份的三件套。transcript + memory 两份 `VACUUM INTO`，加一份清单；
//! **7 组**轮转、失败置位、清单水位与记忆摘要可验证。
//! 账本永不出中心：备份里没有 `ledger.db`、没有路径。
use rc_ledger::water::{self, Cursor};
use rc_store::Root;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hub binary")
}

fn turn_uid(i: u32) -> String {
    format!("{:064x}", i + 1)
}

/// 合成一轮转录。`turn_uid` 按位编号；`ingested_at_ms` 随编号递增。
fn synth_turn(t: &rc_store::TranscriptHandle, i: u32, session: &str, device: &str) {
    t.conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
             VALUES(?1, ?2, ?3, 'human', '合成正文', '合成指纹', 1, NULL, ?4, ?4)",
            rusqlite::params![turn_uid(i), session, device, 1000 + i as i64],
        )
        .unwrap();
    t.conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine,
                                  project_hint, last_saved_at_ms)
             VALUES(?1, ?2, 'agent', NULL, NULL, ?3)
             ON CONFLICT(session_key, device_id)
             DO UPDATE SET last_saved_at_ms=excluded.last_saved_at_ms",
            rusqlite::params![session, device, 1000 + i as i64],
        )
        .unwrap();
}

/// 写一张卡（带一条人工轮出处）。记忆行数因此至少是 `card` 1 + `provenance` 1。
fn put_card(m: &rc_store::MemoryHandle, uid: &str, title: &str, now_ms: i64) -> String {
    rc_memory::writers_x::put_card(
        m,
        &rc_memory::FactContent {
            kind: "note".into(),
            title: title.into(),
            body: format!("{title} 的合成正文"),
            scope: rc_memory::Scope {
                kind: rc_memory::ScopeKind::Project,
                key: "proj-backup".into(),
            },
            valid_to_ms: None,
        },
        &[rc_memory::Provenance {
            turn_uid: uid.to_string(),
            anchor_role: rc_memory::AnchorRole::Human,
        }],
        now_ms,
    )
    .unwrap()
    .id
}

fn data_arg(data: &Path) -> String {
    data.to_str().unwrap().to_string()
}

fn backup_once(data: &Path, now_ms: i64) -> (PathBuf, Output) {
    let dd = data_arg(data);
    let ms = now_ms.to_string();
    let out = run(&["backup", "--data-dir", &dd, "--now-ms", &ms]);
    let manifest = data.join("backups").join(format!("backup-{now_ms}.json"));
    (manifest, out)
}

fn entry_names(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn counter(root: &Root, metric: &str) -> Option<i64> {
    rc_ledger::counters::read_all(root.ledger())
        .unwrap()
        .into_iter()
        .find(|r| {
            r.subject_kind == "maintenance" && r.subject_id == "backup" && r.metric == metric
        })
        .map(|r| r.count)
}

fn table_names(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open(path).unwrap();
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    let rows = st
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    rows
}

fn digest_of(conn: &rusqlite::Connection) -> String {
    rc_store::snapshot::logical_digest(conn, rc_memory::schema::MEMORY_TABLES).unwrap()
}

fn memory_rows_of(conn: &rusqlite::Connection) -> i64 {
    rc_memory::schema::MEMORY_TABLES
        .iter()
        .map(|t| {
            conn.query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        })
        .sum()
}

#[test]
fn backup_writes_two_copies_and_a_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let source_digest;
    let source_rows;
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        synth_turn(root.transcript(), 1, "sess-a", "dev-a");
        put_card(root.memory(), &turn_uid(0), "备份点上的卡", 1000);
        source_digest = digest_of(root.memory().conn());
        source_rows = memory_rows_of(root.memory().conn());
    }
    let (manifest_path, out) = backup_once(&data, 5000);
    assert_eq!(
        out.status.code(),
        Some(0),
        "备份必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["backup"], true);
    assert_eq!(doc["name"], "transcript-5000.db");
    assert_eq!(doc["memory"], "memory-5000.db");
    assert_eq!(doc["memory_rows"], source_rows, "输出里的记忆行数必须与源库一致");

    let transcript = data.join("backups").join("transcript-5000.db");
    let memory = data.join("backups").join("memory-5000.db");
    assert!(transcript.is_file(), "转录备份不在：{transcript:?}");
    assert!(memory.is_file(), "记忆备份不在：{memory:?}");
    assert!(manifest_path.is_file(), "清单不在：{manifest_path:?}");

    let manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).expect("清单必须是 JSON");
    assert_eq!(manifest["transcript"], "transcript-5000.db");
    assert_eq!(manifest["memory"], "memory-5000.db");

    let conn = rusqlite::Connection::open(&memory).unwrap();
    let names = table_names(&memory);
    for table in rc_memory::schema::MEMORY_TABLES {
        assert!(
            names.iter().any(|n| n == table),
            "记忆备份缺表 {table}：{names:?}"
        );
    }
    assert!(
        !names.iter().any(|n| n == "turns" || n == "sessions"),
        "记忆备份里出现了转录表：{names:?}"
    );
    let cards: i64 = conn
        .query_row("SELECT count(*) FROM card", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cards, 1, "记忆备份里没有备份点上的那张卡");
    assert_eq!(
        digest_of(&conn),
        source_digest,
        "记忆备份的逻辑摘要必须与源库一致"
    );

    let root = Root::open(&data).unwrap();
    assert_eq!(counter(&root, "backup_ok_at_ms"), Some(5000));
    assert_eq!(
        counter(&root, "backup_fail_at_ms"),
        None,
        "成功的那一次不许留下失败时刻"
    );
}

#[test]
fn backup_keeps_seven_sets_and_drops_the_oldest() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(data.join("backups")).unwrap();
    std::fs::write(data.join("backups").join("notes.txt"), b"not a backup").unwrap();
    let mut codes = Vec::new();
    for now_ms in 1..=8 {
        let (_, out) = backup_once(&data, now_ms);
        codes.push(out.status.code());
    }
    assert_eq!(
        codes.iter().filter(|c| **c == Some(0)).count(),
        8,
        "八次备份必须全部成功：{codes:?}"
    );
    let names = entry_names(&data.join("backups"));
    let manifests: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("backup-") && n.ends_with(".json"))
        .collect();
    assert_eq!(manifests.len(), 7, "备份必须恰好留七组：{names:?}");
    for gone in ["backup-1.json", "transcript-1.db", "memory-1.db"] {
        assert!(
            !names.iter().any(|n| n == gone),
            "最旧那一组必须整组被轮转掉（{gone} 还在）：{names:?}"
        );
    }
    for kept in ["backup-8.json", "transcript-8.db", "memory-8.db"] {
        assert!(
            names.iter().any(|n| n == kept),
            "最新那一组必须在（{kept} 不在）：{names:?}"
        );
    }
    assert!(
        names.iter().any(|n| n == "notes.txt"),
        "认不出的名字不许碰：{names:?}"
    );
}

#[test]
fn a_failed_backup_is_recorded_and_exits_three() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data-canary-backup-fail-7719");
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
    }
    let (manifest, first) = backup_once(&data, 5000);
    assert_eq!(first.status.code(), Some(0), "第一次备份必须成功");
    assert!(manifest.is_file());
    // 同一个 --now-ms：目标已存在 ⇒ 失败分支；旧组一个字节都不许动。
    let (_, second) = backup_once(&data, 5000);
    assert_eq!(
        second.status.code(),
        Some(3),
        "备份失败必须退出 3：{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        !stderr.contains(&data_arg(&data)),
        "失败串带了数据目录路径：{stderr}"
    );

    let root = Root::open(&data).unwrap();
    assert_eq!(
        counter(&root, "backup_fail_at_ms"),
        Some(5000),
        "备份失败必须把失败时刻置进 backup_fail_at_ms"
    );
    let names = entry_names(&data.join("backups"));
    let manifests: Vec<&String> = names
        .iter()
        .filter(|n| n.starts_with("backup-") && n.ends_with(".json"))
        .collect();
    assert_eq!(manifests.len(), 1, "失败的那一次不许留下第二组：{names:?}");
    let transcripts = names
        .iter()
        .filter(|n| n.starts_with("transcript-") && n.ends_with(".db"))
        .count();
    let memories = names
        .iter()
        .filter(|n| n.starts_with("memory-") && n.ends_with(".db"))
        .count();
    assert_eq!((transcripts, memories), (1, 1), "旧组被写坏了：{names:?}");
    // 旧组仍然可用：清单与两份副本都在。
    assert!(manifest.is_file());
    assert!(data.join("backups").join("transcript-5000.db").is_file());
    assert!(data.join("backups").join("memory-5000.db").is_file());
}

// ── S1b-C2a：归属侧表跟着记忆备份走 ───────────────────────────────────────
// 判据：`resource_owner` / `resource_grant` 是 canonical 表，备份必须带上它们的行，
// 且逐列一致（列序由 `CREATE TABLE` 与 `SELECT *` 共同钉住）。

/// 全套列读成值向量：`SELECT *` 的列序就是建表列序，逐列比较不会漏列。
fn all_rows(conn: &rusqlite::Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
    let mut st = conn.prepare(&format!("SELECT * FROM \"{table}\"")).unwrap();
    let n = st.column_count();
    st.query_map([], |r| {
        let mut values = Vec::with_capacity(n);
        for i in 0..n {
            values.push(r.get::<_, rusqlite::types::Value>(i)?);
        }
        Ok(values)
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn put_owner_row(m: &rc_store::MemoryHandle, mem_id: &str) {
    // S1b-C3 起 `put_card` 自己就写一行合成归属（owner=synthetic-agent、private、default）。
    // 夹具要的是"带 retention/scope 的归属行"，所以先替掉合成的这一行——同一 `(mem_table,mem_id)`
    // 只允许一行（PK），第二次 INSERT 会被拒。备份侧断的是"副本与源库逐列一致"，与这行的来源无关。
    m.conn()
        .execute(
            "DELETE FROM resource_owner WHERE mem_table='card' AND mem_id=?1",
            rusqlite::params![mem_id],
        )
        .unwrap();
    m.conn()
        .execute(
            "INSERT INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                        created_by_agent, visibility, retention, scope_kind,
                                        scope_key, source_agent, device_id, created_at_ms)
             VALUES('card', ?1, 'p-1', 'agent-a', 'agent-a', 'private_agent', 'days:7',
                    'project', 'proj-backup', 'agent-a', 'dev-a', 1000)",
            rusqlite::params![mem_id],
        )
        .unwrap();
}

fn put_grant_row(m: &rc_store::MemoryHandle, id: &str) {
    m.conn()
        .execute(
            "INSERT INTO resource_grant(id, from_principal_id, from_agent_id, to_agent_id,
                                        to_principal_id, capability, data_class, scope_kind,
                                        scope_key, purpose, granted_at_ms, expires_at_ms, revoked_at_ms)
             VALUES(?1, 'p-1', 'agent-a', 'agent-b', NULL, 'recall', 'memory', 'project',
                    'proj-backup', 'share the card', 1000, 2000, NULL)",
            rusqlite::params![id],
        )
        .unwrap();
}

#[test]
fn a_memory_backup_carries_the_ownership_side_tables() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let (source_owner, source_grant);
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        let m = root.memory();
        let card = put_card(m, &turn_uid(0), "有归属的卡", 1000);
        put_owner_row(m, &card);
        put_grant_row(m, "grant-1");
        source_owner = all_rows(m.conn(), "resource_owner");
        source_grant = all_rows(m.conn(), "resource_grant");
        assert_eq!(source_owner.len(), 1, "夹具的归属行没写进去");
        assert_eq!(source_grant.len(), 1, "夹具的授权行没写进去");
    }
    let (manifest, out) = backup_once(&data, 5000);
    assert_eq!(
        out.status.code(),
        Some(0),
        "备份必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(manifest.is_file());
    let memory = data.join("backups").join("memory-5000.db");
    let copy = rusqlite::Connection::open(&memory).unwrap();
    assert_eq!(
        all_rows(&copy, "resource_owner"),
        source_owner,
        "记忆备份里的 resource_owner 与源库不是逐列一致"
    );
    assert_eq!(
        all_rows(&copy, "resource_grant"),
        source_grant,
        "记忆备份里的 resource_grant 与源库不是逐列一致"
    );
    // 备份清单的行数与摘要覆盖了这两张表（空表也算在里面，所以这里只能是源库读数）。
    let doc: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(doc["memory_rows"], memory_rows_of(&copy));
}

#[test]
fn the_manifest_records_watermarks_and_the_memory_digest() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let scope = water::distill_scope("dev-a", "sess-a");
    let uid = turn_uid(0);
    let source_digest;
    let source_rows;
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        put_card(root.memory(), &uid, "水位旁的那张卡", 1000);
        water::advance(
            root.ledger(),
            "distill",
            &scope,
            &Cursor {
                at_ms: 1000,
                turn_uid: uid.clone(),
            },
            1000,
        )
        .unwrap();
        source_digest = digest_of(root.memory().conn());
        source_rows = memory_rows_of(root.memory().conn());
    }
    let (manifest_path, out) = backup_once(&data, 9000);
    assert_eq!(
        out.status.code(),
        Some(0),
        "备份必须成功退出：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).expect("清单必须是 JSON");
    assert_eq!(doc["format_version"], 1);
    assert_eq!(doc["created_at_ms"], 9000);
    assert_eq!(doc["memory_version"], 6);
    assert_eq!(doc["transcript_max_ingested_at"], 1000);
    assert_eq!(doc["memory_rows"], source_rows);
    assert_eq!(doc["memory_digest"], source_digest.as_str());
    let watermarks = doc["distill_watermarks"]
        .as_array()
        .expect("distill_watermarks 必须是数组");
    assert_eq!(watermarks.len(), 1, "清单里的水位必须恰好一条：{watermarks:?}");
    assert_eq!(watermarks[0]["scope"], scope);
    assert_eq!(watermarks[0]["at_ms"], 1000);
    assert_eq!(watermarks[0]["turn_uid"], uid);

    // 摘要与行数可以从记忆副本**重算**：不是只信清单自己写下的值。
    let copy = rusqlite::Connection::open(data.join("backups").join("memory-9000.db")).unwrap();
    assert_eq!(digest_of(&copy), source_digest);
    assert_eq!(memory_rows_of(&copy), source_rows);
}
