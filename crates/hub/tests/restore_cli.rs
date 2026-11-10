// crates/hub/tests/restore_cli.rs
//! T5-B / B5a：按清单恢复。① 校验；② 等显式确认；③ 替换两份备份、
//! 回填记忆库（含判定族）、重建索引、把蒸馏水位设到清单停点（**不归零**）。
//! 备份由 `hub backup` 自己产出——这一组只钉"恢复"，不重造清单格式。
use rc_ledger::water::{self, Cursor};
use rc_memory::writers_a::attest;
use rc_memory::writers_c::retire;
use rc_memory::writers_x::put_card;
use rc_memory::{
    AnchorRole, FactContent, Provenance, Scope, ScopeKind, Stance,
};
use rc_store::Root;
use serde_json::Value;
use std::path::Path;
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

fn put_card_row(
    m: &rc_store::MemoryHandle,
    uid: &str,
    title: &str,
    scope_key: &str,
    now_ms: i64,
) -> String {
    put_card(
        m,
        &FactContent {
            kind: "note".into(),
            title: title.into(),
            body: format!("{title} 的合成正文"),
            scope: Scope {
                kind: ScopeKind::Project,
                key: scope_key.into(),
            },
            valid_to_ms: None,
        },
        &[Provenance {
            turn_uid: uid.to_string(),
            anchor_role: AnchorRole::Human,
        }],
        now_ms,
    )
    .unwrap()
    .id
}

fn data_arg(data: &Path) -> String {
    data.to_str().unwrap().to_string()
}

/// 跑一次 `hub backup`，返回清单路径。退出码不是 0 就是夹具坏了。
fn backup_cli(data: &Path, now_ms: i64) -> std::path::PathBuf {
    let dd = data_arg(data);
    let ms = now_ms.to_string();
    let out = run(&["backup", "--data-dir", &dd, "--now-ms", &ms]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "夹具备份必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    data.join("backups").join(format!("backup-{now_ms}.json"))
}

fn count_turns(root: &Root) -> i64 {
    root.transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap()
}

fn counter(root: &Root, metric: &str) -> Option<i64> {
    rc_ledger::counters::read_all(root.ledger())
        .unwrap()
        .into_iter()
        .find(|r| {
            r.subject_kind == "maintenance" && r.subject_id == "restore" && r.metric == metric
        })
        .map(|r| r.count)
}

fn count_restore_audits(root: &Root) -> i64 {
    root.ledger()
        .conn()
        .query_row(
            "SELECT count(*) FROM audit_log WHERE action='restore' AND detail='restored'",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn memory_row_count(m: &rc_store::MemoryHandle) -> i64 {
    rc_memory::schema::MEMORY_TABLES
        .iter()
        .map(|t| {
            m.conn()
                .query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| {
                    r.get::<_, i64>(0)
                })
                .unwrap()
        })
        .sum()
}

#[test]
fn restore_without_confirm_stops_after_the_check() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
    }
    let manifest = backup_cli(&data, 5000);
    {
        // 备份之后再加一轮：没确认就恢复的话，这一行会被覆盖掉。
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 1, "sess-a", "dev-a");
    }
    let dd = data_arg(&data);
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "只做校验不是失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // 行数先断：这一步的牙是"没确认就不许覆盖"，不是那一串 JSON 的形状。
    let root = Root::open(&data).unwrap();
    assert_eq!(
        count_turns(&root),
        2,
        "没给 --confirm 就覆盖了转录层（备份里只有 1 行）"
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["checked"], true);
    assert_eq!(doc["turns"], 1);
    assert_eq!(doc["memory_rows"], 0);
    assert_eq!(doc["awaiting_confirm"], true);
}

#[test]
fn restore_brings_back_memory_and_judgment_rows() {
    let dir = tempfile::tempdir().unwrap();
    // 数据目录名带金丝雀：备份路径在它下面一层，路径一旦被打出来必然命中。
    let data = dir.path().join("data-canary-restore-path-9917");
    let uid = turn_uid(0);
    let (card_id, attestation_id);
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        let m = root.memory();
        let key = rc_memory::MemKey {
            table: rc_memory::MemTable::Card,
            id: put_card_row(m, &uid, "备份点上的卡", "proj-restore", 1000),
        };
        card_id = key.id.clone();
        attest(m, &key, Stance::Confirmed, Some(&uid), Some("kept"), 1000).unwrap();
        retire(m, &key, "manual", 1000).unwrap();
        attestation_id = m
            .conn()
            .query_row("SELECT id FROM attestation", [], |r| r.get::<_, i64>(0))
            .unwrap();
    }
    let manifest = backup_cli(&data, 6000);
    {
        // 备份之后写第二张卡与它自己的判定账：restore 必须把这两行也丢掉。
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 1, "sess-a", "dev-a");
        let m = root.memory();
        let key = rc_memory::MemKey {
            table: rc_memory::MemTable::Card,
            id: put_card_row(m, &turn_uid(1), "备份之后的卡", "proj-restore", 2000),
        };
        attest(m, &key, Stance::Rejected, None, None, 2000).unwrap();
        retire(m, &key, "manual", 2000).unwrap();
    }
    let dd = data_arg(&data);
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "确认之后恢复必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["restored"], true);
    assert_eq!(doc["turns"], 1);
    assert_eq!(
        doc["memory_rows"],
        5,
        "备份点的记忆行数必须如实回报（card+provenance+attestation+retirement 四条 + C3 的归属行一条）"
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        !text.contains(&dd),
        "输出带了数据目录路径（备份文件的完整路径必在其中）：{text}"
    );

    let root = Root::open(&data).unwrap();
    let m = root.memory();
    assert_eq!(count_turns(&root), 1, "恢复之后转录行数必须回到备份那一刻");
    let cards: Vec<String> = {
        let mut st = m.conn().prepare("SELECT id FROM card ORDER BY id").unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(cards, vec![card_id.clone()], "备份点之后的卡没有被丢掉");
    let attestation: (i64, String, String) = m
        .conn()
        .query_row(
            "SELECT id, stance, mem_id FROM attestation",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        attestation,
        (attestation_id, "confirmed".to_string(), card_id.clone()),
        "判定族行没有带着原 id 回来"
    );
    let retirements: Vec<(String, String)> = {
        let mut st = m
            .conn()
            .prepare("SELECT mem_table, mem_id FROM retirement")
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    assert_eq!(
        retirements,
        vec![("card".to_string(), card_id)],
        "退场账没有回到备份点"
    );
    assert_eq!(
        memory_row_count(m),
        5,
        "记忆库整替之后的行数必须等于备份点（C3 起含 put_card 写的归属行）"
    );
    assert!(
        rc_index::index_status(m).unwrap().is_some(),
        "恢复之后索引层必须重建"
    );
    assert_eq!(counter(&root, "restored_turns"), Some(1));
    assert_eq!(
        count_restore_audits(&root),
        1,
        "恢复必须恰好留一行 restore+restored 审计"
    );
}

#[test]
fn restore_sets_the_distill_cursor_to_the_manifest_point() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let uid = turn_uid(0);
    let scope = water::distill_scope("dev-a", "sess-a");
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
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
    }
    let manifest = backup_cli(&data, 7000);
    {
        // 备份之后把水位推过下一轮：restore 必须把它**设回**清单里的停点。
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 1, "sess-a", "dev-a");
        water::advance(
            root.ledger(),
            "distill",
            &scope,
            &Cursor {
                at_ms: 1001,
                turn_uid: turn_uid(1),
            },
            1001,
        )
        .unwrap();
    }
    let dd = data_arg(&data);
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "确认之后恢复必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    {
        let root = Root::open(&data).unwrap();
        let l = root.ledger();
        assert_eq!(
            water::read(l, "distill", &scope).unwrap(),
            Some(Cursor {
                at_ms: 1000,
                turn_uid: uid,
            }),
            "恢复必须把蒸馏游标设到清单停点（不是归零）"
        );
        assert!(
            !water::halted(l, "distill", &scope).unwrap(),
            "恢复必须解除停轮"
        );
    }
    // 停点之后的第一轮：备份覆盖的 turn 不重蒸。
    let replies = dir.path().join("empty-replies.json");
    std::fs::write(&replies, "[]").unwrap();
    let out = run(&[
        "cycle",
        "once",
        "--replies",
        replies.to_str().unwrap(),
        "--data-dir",
        &dd,
    ]);
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(
        doc["turns_scanned"], 0,
        "备份覆盖的 turn 被重新蒸了（水位没有停在清单点）"
    );
    assert_eq!(
        out.status.code(),
        Some(0),
        "空批的 cycle 必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── S1b-C1：会话归属 7 列与恢复兼容 ───────────────────────────────────────
// 判据：v4 转录备份（6 列 sessions）仍可恢复且 7 列落 legacy 默认；当前 v5 备份
// 往返回归逐列一致；其它版本照旧拒绝。

type Ownership = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn owned(
    principal: Option<&str>,
    agent: &str,
    kind: &str,
    key: &str,
    visibility: &str,
    retention: &str,
    src: &str,
) -> Ownership {
    (
        principal.map(str::to_string),
        Some(agent.to_string()),
        Some(kind.to_string()),
        Some(key.to_string()),
        Some(visibility.to_string()),
        Some(retention.to_string()),
        Some(src.to_string()),
    )
}

fn session_ownership(root: &Root, session: &str, device: &str) -> Ownership {
    root.transcript()
        .conn()
        .query_row(
            "SELECT owner_principal_id, owner_agent_id, scope_kind, scope_key,
                    visibility, retention, ownership_src
             FROM sessions WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![session, device],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .unwrap()
}

fn set_explicit_ownership(root: &Root) {
    root.transcript()
        .conn()
        .execute(
            "UPDATE sessions SET owner_principal_id='p-9', owner_agent_id='agent-x',
                    scope_kind='org', scope_key='org:k', visibility='org',
                    retention='days:7', ownership_src='operator'
             WHERE session_key='sess-a'",
            [],
        )
        .unwrap();
}

/// 备份目录里那份转录库的路径（清单只存文件名）。
fn manifest_transcript_file(manifest: &Path) -> std::path::PathBuf {
    let doc: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    let name = doc["transcript"].as_str().expect("清单缺 transcript 文件名");
    manifest.parent().unwrap().join(name)
}

/// 把当前产物打出的转录备份退回 v4 形状：删掉归属索引与 7 列、版本压回 4。
/// 这样得到的是**形状自证**的旧备份，而不是只把 user_version 改成 4 的假旧库。
fn downgrade_transcript_backup_to_v4(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "DROP INDEX IF EXISTS sessions_by_owner;
         ALTER TABLE sessions DROP COLUMN owner_principal_id;
         ALTER TABLE sessions DROP COLUMN owner_agent_id;
         ALTER TABLE sessions DROP COLUMN scope_kind;
         ALTER TABLE sessions DROP COLUMN scope_key;
         ALTER TABLE sessions DROP COLUMN visibility;
         ALTER TABLE sessions DROP COLUMN retention;
         ALTER TABLE sessions DROP COLUMN ownership_src;
         PRAGMA user_version=4;",
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM pragma_table_info('sessions')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 6, "夹具没能退回 v4 形状（sessions 应剩 6 列）");
}

#[test]
fn restoring_a_v4_transcript_backup_fills_legacy_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        // v4 中心的会话行只有 project_hint，没有归属列；备份之后这行会被丢掉。
        root.transcript()
            .conn()
            .execute("UPDATE sessions SET project_hint='proj-a' WHERE session_key='sess-a'", [])
            .unwrap();
    }
    let manifest = backup_cli(&data, 8000);
    let backup_db = manifest_transcript_file(&manifest);
    downgrade_transcript_backup_to_v4(&backup_db);

    // 备份之后再加一行：恢复成功的话它必须消失（证明整替真的发生了）。
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 1, "sess-b", "dev-a");
    }
    let dd = data_arg(&data);
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "v4 转录备份必须仍可恢复：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["turns"], 1);

    let root = Root::open(&data).unwrap();
    assert_eq!(count_turns(&root), 1, "备份之后的那一行没有被丢掉");
    assert_eq!(
        session_ownership(&root, "sess-a", "dev-a"),
        owned(
            None,
            "agent",
            "project",
            "proj-a",
            "private_agent",
            "default",
            "legacy"
        ),
        "v4 备份恢复后的归属必须是 legacy 默认（owner=source_agent、private、default）"
    );
    let version: i32 = root
        .transcript()
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, rc_store::ddl::TRANSCRIPT_VERSION, "恢复后的库必须还是当前版本");
}

#[test]
fn a_v5_round_trip_keeps_every_ownership_column() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let explicit = owned(Some("p-9"), "agent-x", "org", "org:k", "org", "days:7", "operator");
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        set_explicit_ownership(&root);
        assert_eq!(session_ownership(&root, "sess-a", "dev-a"), explicit);
    }
    let manifest = backup_cli(&data, 9000);

    let mut target = Root::open(&dir.path().join("restored")).unwrap();
    hub::restore::restore(&mut target, &manifest, 9500).unwrap();
    assert_eq!(count_turns(&target), 1);
    assert_eq!(
        session_ownership(&target, "sess-a", "dev-a"),
        explicit,
        "v5 备份往返丢了归属列"
    );
}

#[test]
fn a_transcript_backup_from_another_version_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    for (i, version) in [3_i32, 6_i32].into_iter().enumerate() {
        let data = dir.path().join(format!("data-{i}"));
        {
            let root = Root::open(&data).unwrap();
            synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        }
        let now = 10_000 + i as i64;
        let manifest = backup_cli(&data, now);
        let backup_db = manifest_transcript_file(&manifest);
        {
            let conn = rusqlite::Connection::open(&backup_db).unwrap();
            conn.pragma_update(None, "user_version", version).unwrap();
        }
        let dd = data_arg(&data);
        let out = run(&[
            "restore",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            "--data-dir",
            &dd,
        ]);
        assert_eq!(
            out.status.code(),
            Some(3),
            "版本 {version} 的转录备份必须被拒（只接受 4 与当前版本）"
        );
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(text.contains("the backup does not check out"), "{text}");
        // 拒绝发生在任何覆盖之前：活库里的那一行还在。
        let root = Root::open(&data).unwrap();
        assert_eq!(count_turns(&root), 1, "被拒的恢复不该动活库");
    }
}

// ── S1b-C2a：旧 25 表记忆备份仍可恢复 ─────────────────────────────────────
// 判据：备份缺 `resource_owner`/`resource_grant` 时，摘要/行数只按**备份实际存在的表**
// 重算 ⇒ 旧清单照旧对得上；restore 成功且两张新表在活库里是空的。
// 反向：删掉一张**旧**表（card）的坏备份仍必须被摘要/行数校验拒绝——present-only
// 不是"缺什么都放过"，它只对"清单自己也没算过的表"宽容。

/// 备份目录里那份记忆库的路径（清单只存文件名）。
fn manifest_memory_file(manifest: &Path) -> std::path::PathBuf {
    let doc: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    let name = doc["memory"].as_str().expect("清单缺 memory 文件名");
    manifest.parent().unwrap().join(name)
}

/// 恢复侧的 present-only 表集：`sqlite_master` 的表名 ∩ `MEMORY_TABLES`，保持清单顺序。
fn present_memory_tables(conn: &rusqlite::Connection) -> Vec<&'static str> {
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table'")
        .unwrap();
    let names: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    rc_memory::schema::MEMORY_TABLES
        .iter()
        .copied()
        .filter(|t| names.iter().any(|n| n == t))
        .collect()
}

/// 按 present-only 口径重算 (摘要, 行数)——与 restore 侧同一算法的夹具版本。
fn memory_readings(conn: &rusqlite::Connection) -> (String, i64) {
    let present = present_memory_tables(conn);
    let digest = rc_store::snapshot::logical_digest(conn, &present).unwrap();
    let rows = present
        .iter()
        .map(|t| {
            conn.query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| r.get::<_, i64>(0))
                .unwrap()
        })
        .sum();
    (digest, rows)
}

fn rewrite_manifest_memory(manifest: &Path, digest: &str, rows: i64) {
    let mut doc: Value = serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
    doc["memory_digest"] = Value::String(digest.to_string());
    doc["memory_rows"] = Value::from(rows);
    std::fs::write(manifest, serde_json::to_vec(&doc).unwrap()).unwrap();
}

fn put_owner_row(m: &rc_store::MemoryHandle, mem_id: &str) {
    // S1b-C3 起 `put_card` 自己就写一行合成归属（owner=synthetic-agent、private、default）。
    // 夹具要的是"带 retention/scope 的归属行"，所以先替掉合成的这一行——同一 `(mem_table,mem_id)`
    // 只允许一行（PK），第二次 INSERT 会被拒。
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
                    'project', 'proj-legacy', 'agent-a', 'dev-a', 1000)",
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
                    'proj-legacy', 'share the card', 1000, 2000, NULL)",
            rusqlite::params![id],
        )
        .unwrap();
}

fn table_count(m: &rc_store::MemoryHandle, table: &str) -> i64 {
    m.conn()
        .query_row(&format!("SELECT count(*) FROM \"{table}\""), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_legacy_25_table_memory_backup_still_restores() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let uid = turn_uid(0);
    let legacy_rows;
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        let m = root.memory();
        let card = put_card_row(m, &uid, "旧备份里的卡", "proj-legacy", 1000);
        put_owner_row(m, &card);
        put_grant_row(m, "grant-1");
    }
    let manifest = backup_cli(&data, 11_000);
    let backup_db = manifest_memory_file(&manifest);
    {
        // 退回 C2a 之前的 25 表形状：两张归属侧表与 S3a 的两张审查侧表被 DROP，
        // 再加 W15a 的背书事件表、G1 刀 B 的归档章表与波二刀 A 的复用事件表
        // （它们也是备份点之后才有的侧表），清单按 present-only 口径重算。
        let conn = rusqlite::Connection::open(&backup_db).unwrap();
        conn.execute_batch(
            "DROP TABLE resource_owner;
             DROP TABLE resource_grant;
             DROP TABLE review_item;
             DROP TABLE review_decision;
             DROP TABLE card_verdict_event;
             DROP TABLE card_sink;
             DROP TABLE card_reuse_event;",
        )
        .unwrap();
        assert_eq!(present_memory_tables(&conn).len(), 25, "夹具没能退回 25 表形状");
        let (digest, rows) = memory_readings(&conn);
        legacy_rows = rows;
        drop(conn);
        rewrite_manifest_memory(&manifest, &digest, rows);
    }
    assert_eq!(legacy_rows, 2, "旧 25 表备份里应当只有 card 1 + provenance 1");

    // 备份之后再加一张卡：恢复成功的话它必须消失（证明整替真的发生了）。
    {
        let root = Root::open(&data).unwrap();
        put_card_row(root.memory(), &turn_uid(1), "备份之后的卡", "proj-legacy", 2000);
    }
    let dd = data_arg(&data);
    // ① 只校验不确认：旧备份必须过校验，并如实回报 present-only 行数。
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "25 表记忆备份必须能过 restore --check：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["checked"], true);
    assert_eq!(doc["memory_rows"], legacy_rows, "校验回报的行数不是 present-only 读数");
    assert_eq!(doc["awaiting_confirm"], true);

    // ② 确认恢复：成功，旧表行数等于备份点，两张新表存在且为空。
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "25 表记忆备份必须能恢复：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc: Value = serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(doc["restored"], true);
    assert_eq!(doc["memory_rows"], legacy_rows);

    let root = Root::open(&data).unwrap();
    let m = root.memory();
    assert_eq!(table_count(m, "card"), 1, "备份点之后的卡没有被丢掉");
    assert_eq!(
        memory_row_count(m),
        legacy_rows,
        "恢复后 30 张表的行数之和必须等于旧备份的 25 表读数（新表为空）"
    );
    // 备份里写过归属/授权行，旧备份没带它们 ⇒ 恢复后必须是空表，不是"活库里的旧值"。
    assert_eq!(table_count(m, "resource_owner"), 0, "旧备份恢复后 resource_owner 不是空的");
    assert_eq!(table_count(m, "resource_grant"), 0, "旧备份恢复后 resource_grant 不是空的");
    // 当前库仍是 32 张表、版本仍是 6（新表靠 IF NOT EXISTS 幂等重建，不 bump 版本）。
    let present = {
        let conn = m.conn();
        let mut st = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap();
        let names: Vec<String> = st
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        rc_memory::schema::MEMORY_TABLES
            .iter()
            .filter(|t| names.iter().any(|n| n == **t))
            .count()
    };
    assert_eq!(present, 32, "恢复后的活库必须还有全部 32 张 canonical 表");
    let version: i32 = m
        .conn()
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap();
    assert_eq!(version, rc_store::ddl::MEMORY_VERSION);
}

/// G1 刀 B 第 0 件：归档章是**消费面状态**，必须随备份整替恢复（不像 `review_verdict`
/// 那样是可重跑的证据附件）。程序章可回填再生，LLM 价值轴的章（`reason='value_axis'`）
/// 不可再生——备份里丢了就永久丢了，所以它必须在 `MEMORY_TABLES` 里。
#[test]
fn card_sink_stamps_survive_a_memory_restore() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let uid = turn_uid(0);
    let stamped;
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        let m = root.memory();
        let card = put_card_row(m, &uid, "备份里带章的卡", "proj-sink", 1000);
        rc_memory::value_gate::mark_archive(m.conn(), &card, "value_axis", 1000, 1).unwrap();
        stamped = card;
    }
    let manifest = backup_cli(&data, 11_000);
    // 备份之后再加一张卡、再盖一枚章：恢复成功的话两者都必须消失（证明整替真的发生了）。
    {
        let root = Root::open(&data).unwrap();
        let m = root.memory();
        let later = put_card_row(m, &uid, "备份之后的卡", "proj-sink", 2000);
        rc_memory::value_gate::mark_archive(m.conn(), &later, "program_gate", 2000, 1).unwrap();
    }
    let dd = data_arg(&data);
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "带 card_sink 的备份必须能恢复：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let root = Root::open(&data).unwrap();
    let m = root.memory();
    assert_eq!(table_count(m, "card"), 1, "备份点之后的卡没有被整替掉");
    assert_eq!(table_count(m, "card_sink"), 1, "备份点之后的章没有被整替掉");
    let (id, sink, reason): (String, String, String) = m
        .conn()
        .query_row("SELECT card_id, sink, reason FROM card_sink", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(id, stamped, "恢复出来的章不是备份点那枚");
    assert_eq!(sink, "archive");
    assert_eq!(reason, "value_axis", "LLM 章不可再生：必须逐字随备份恢复");
    assert!(
        rc_memory::value_gate::archived_card_ids(m).unwrap().contains(&stamped),
        "恢复后召回读口立刻看得见这张归档卡"
    );
}

#[test]
fn a_memory_backup_missing_an_old_table_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        put_card_row(root.memory(), &turn_uid(0), "会被删掉的那张卡", "proj-bad", 1000);
    }
    let manifest = backup_cli(&data, 12_000);
    let backup_db = manifest_memory_file(&manifest);
    {
        // 只删旧表、**不改清单**：present-only 口径算出的摘要会少一张表 ⇒ 必须对不上。
        let conn = rusqlite::Connection::open(&backup_db).unwrap();
        conn.execute_batch("DROP TABLE card;").unwrap();
    }
    let dd = data_arg(&data);
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "缺一张旧表的坏备份必须被拒：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("the backup does not check out"), "{text}");
    let root = Root::open(&data).unwrap();
    assert_eq!(
        table_count(root.memory(), "card"),
        1,
        "被拒的恢复不该动活库"
    );
}


/// 波二刀 A：复用反馈是**不可再生的消费面状态**（"这张卡帮没帮上忙"只发生过一次，
/// 没有任何程序能回填）。它在 `MEMORY_TABLES` 里 ⇒ 必须随备份整替恢复；备份点之后
/// 追加的反馈要被整替掉（证明整替真的发生了）。
#[test]
fn card_reuse_events_survive_a_memory_restore() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let uid = turn_uid(0);
    let kept;
    {
        let root = Root::open(&data).unwrap();
        synth_turn(root.transcript(), 0, "sess-a", "dev-a");
        let m = root.memory();
        kept = put_card_row(m, &uid, "备份里带反馈的卡", "proj-reuse", 1000);
        rc_memory::reuse::record_reuse(m, &kept, "worked", Some("备份点前"), 1000, "agent-a")
            .unwrap();
    }
    let manifest = backup_cli(&data, 13_000);
    // 备份之后再加一张卡、再记一条反馈：恢复成功的话两者都必须消失。
    {
        let root = Root::open(&data).unwrap();
        let m = root.memory();
        let later = put_card_row(m, &uid, "备份之后的卡", "proj-reuse", 2000);
        rc_memory::reuse::record_reuse(m, &later, "failed", Some("备份点后"), 2000, "agent-a")
            .unwrap();
    }
    let dd = data_arg(&data);
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--confirm",
        "--data-dir",
        &dd,
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "带 card_reuse_event 的备份必须能恢复：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let root = Root::open(&data).unwrap();
    let m = root.memory();
    assert_eq!(table_count(m, "card"), 1, "备份点之后的卡没有被整替掉");
    assert_eq!(table_count(m, "card_reuse_event"), 1, "备份点之后的反馈没有被整替掉");
    let (card_id, outcome, note): (String, String, Option<String>) = m
        .conn()
        .query_row("SELECT card_id, outcome, note FROM card_reuse_event", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap();
    assert_eq!(card_id, kept, "恢复出来的反馈不是备份点那一条");
    assert_eq!(outcome, "worked");
    assert_eq!(note.as_deref(), Some("备份点前"), "反馈的说明必须逐字随备份恢复");
    // 读口在恢复后立刻读得到（刀 B 的消费面同一个入口）。
    let counts = rc_memory::reuse::reuse_counts(m, &kept).unwrap();
    assert_eq!(counts.worked, 1, "恢复后按卡聚合立刻看得见这条 worked");
}
