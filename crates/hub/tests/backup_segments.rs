// crates/hub/tests/backup_segments.rs
//! W28 备份段拆分：**库文件可复制段 / 身份不可复制段**。
//!
//! 可复制段：`ledger.db`（账本：pass 状态/水位/设备/审查队列/审计）**必备**，
//! `core.db`（agent 私房本）**存在才备**——懒库合法缺席，缺席在清单/报告里如实记
//! `core: null`，不造假文件。身份密钥（`keys/core_<h>.key`）**永不进备份组**：
//! 身份类密钥不下发的既有裁定，多一份静态拷贝 = 多一类出机器面。
//!
//! 五钉：①组含 ledger+core（无 core 时 core:null）；②加密变体成组、缺一不残留；
//! ③restore round-trip（ledger 行数 + core 文档读回）；④组内文件清单**闭集**断言、
//! 密钥文件不在组内；⑤旧清单（无 ledger/core 段）仍可恢复、版本门照拒。
use rc_core::{CorePayload, CoreStore};
use rc_ledger::water::{self, Cursor};
use rc_store::Root;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn turn_uid(i: u32) -> String {
    format!("{:064x}", i + 1)
}

/// 转录一行 + 账本两处真写入（蒸馏水位、审计行）：ledger.db 的行数从此非零。
fn seed(root: &Root, i: u32) {
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms)
             VALUES(?1, 'sess-a', 'dev-a', 'human', '分段备份正文', 'fp', 1, NULL, 1000, 1000)",
            rusqlite::params![turn_uid(i)],
        )
        .unwrap();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine,
                                  project_hint, last_saved_at_ms)
             VALUES('sess-a','dev-a','agent',NULL,NULL,1000)",
            [],
        )
        .unwrap();
    water::advance(
        root.ledger(),
        "distill",
        &water::distill_scope("dev-a", "sess-a"),
        &Cursor {
            at_ms: 1000,
            turn_uid: turn_uid(i),
        },
        1000,
    )
    .unwrap();
    rc_ledger::device::append_audit(root.ledger(), 1000, "test", "seed", None, "fixture")
        .unwrap();
}

/// agent 私房本：注册 + 写 v1 文档（`l0` 带一行可读回的内容）。
fn seed_core(data: &Path, agent_id: &str, l0_line: &str) {
    let store = CoreStore::open(data).unwrap();
    store.register_agent(agent_id, 1_000).unwrap();
    let mut payload = CorePayload::empty(agent_id);
    payload.l0 = vec![l0_line.to_string()];
    let version = store.save_doc(agent_id, &payload, "dev-a", 1_000).unwrap();
    assert_eq!(version, 1, "夹具的 core 文档必须是 v1");
}

/// 账本读数（备份侧/校验侧同一口径的夹具版）：非 sqlite 内部表 count(*) 之和。
fn ledger_rows_of(path: &Path) -> i64 {
    let conn = ledger_conn(path);
    sum_rows(&conn)
}

fn ledger_conn(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap()
}

fn present_tables(conn: &rusqlite::Connection) -> Vec<String> {
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")
        .unwrap();
    let mut names: Vec<String> = st
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    names.sort();
    names
}

fn sum_rows(conn: &rusqlite::Connection) -> i64 {
    present_tables(conn)
        .iter()
        .map(|t| {
            conn.query_row(&format!("SELECT count(*) FROM \"{t}\""), [], |r| r.get::<_, i64>(0))
                .unwrap()
        })
        .sum()
}

fn core_tables(path: &Path) -> Vec<String> {
    let conn = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    present_tables(&conn)
}

fn dir_snapshot(dir: &Path) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        out.insert(name, bytes);
    }
    out
}

fn backup_now(data: &Path, now_ms: i64) -> PathBuf {
    let mut root = Root::open(data).unwrap();
    hub::backup::backup(&mut root, data, now_ms).unwrap();
    data.join("backups").join(format!("backup-{now_ms}.json"))
}

fn read_manifest(manifest: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap()
}

// ── 钉①+④：组含 ledger+core；无 core 时 core:null；组内文件清单闭集 ──────

#[test]
fn backup_group_carries_ledger_and_core_and_nothing_else() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let source_ledger_rows;
    {
        let root = Root::open(&data).unwrap();
        seed(&root, 0);
        source_ledger_rows = ledger_rows_of(&data.join("ledger.db"));
        assert!(source_ledger_rows > 0, "夹具的账本行数必须非零");
    }
    seed_core(&data, "agent-a", "W28 往返的底线");
    let core_readings = {
        // 备份点的 core 读数：agents=1；digest 用备份侧同一算法的夹具版（全表逐行）。
        let conn = ledger_conn(&data.join("core.db"));
        let agents: i64 = conn
            .query_row("SELECT count(*) FROM core_agents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(agents, 1);
        agents
    };

    let manifest_path = backup_now(&data, 5000);
    let doc = read_manifest(&manifest_path);

    // 清单两段读数：ledger 必在；core 必在且 agents=1。
    assert_eq!(doc["ledger"]["file"], "ledger-5000.db");
    assert_eq!(
        doc["ledger"]["rows"], source_ledger_rows,
        "清单的 ledger 行数必须与源库一致"
    );
    assert!(
        doc["ledger"]["digest"].as_str().is_some_and(|d| d.len() == 64),
        "清单必须带 ledger 内容摘要：{:?}",
        doc["ledger"]
    );
    assert_eq!(doc["core"]["file"], "core-5000.db");
    assert_eq!(doc["core"]["agents"], core_readings);
    assert!(
        doc["core"]["digest"].as_str().is_some_and(|d| d.len() == 64),
        "清单必须带 core 内容摘要：{:?}",
        doc["core"]
    );

    // 组内文件**闭集**：五件，一个不多。
    let mut expect = BTreeMap::new();
    for name in [
        "transcript-5000.db",
        "memory-5000.db",
        "ledger-5000.db",
        "core-5000.db",
        "backup-5000.json",
    ] {
        let path = data.join("backups").join(name);
        assert!(path.is_file(), "组内缺 {name}");
        expect.insert(
            name.to_string(),
            std::fs::metadata(&path).unwrap().len(),
        );
    }
    assert_eq!(
        dir_snapshot(&data.join("backups")),
        expect,
        "备份目录必须恰好是这五件（闭集）：一个多余文件都不许有"
    );

    // 副本与清单自洽（读数可以从副本重算，不是只信清单）。
    let ledger_copy = ledger_rows_of(&data.join("backups").join("ledger-5000.db"));
    assert_eq!(ledger_copy, source_ledger_rows);
    let core_tables = core_tables(&data.join("backups").join("core-5000.db"));
    assert!(
        core_tables.iter().any(|t| t == "core_agents"),
        "core 备份缺注册表：{core_tables:?}"
    );
    for t in &core_tables {
        assert!(
            t.starts_with("core_doc_")
                || t.starts_with("core_rev_")
                || t.starts_with("core_note_")
                || t == "core_agents"
                || t == "core_access_log"
                || t == "core_injection_log",
            "core 备份里出现了不认识的名字 {t}（孤表是合法状态，但本夹具库里不该有）"
        );
    }
}

#[test]
fn core_is_null_when_the_private_store_never_existed() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        seed(&root, 0);
    }
    assert!(
        !data.join("core.db").exists(),
        "夹具库里不该有 core.db（懒库合法缺席的前提）"
    );
    let manifest_path = backup_now(&data, 5000);
    let doc = read_manifest(&manifest_path);

    assert_eq!(
        doc["core"], Value::Null,
        "core.db 缺席时清单必须如实记 core:null，不造假文件"
    );
    assert_eq!(doc["ledger"]["file"], "ledger-5000.db", "ledger 是必备段");

    // 组内闭集：四件（没有 core 的一件），也**没有**被造出来的空 core 备份。
    let names: Vec<String> = dir_snapshot(&data.join("backups")).into_keys().collect();
    assert_eq!(
        names,
        vec![
            "backup-5000.json".to_string(),
            "ledger-5000.db".to_string(),
            "memory-5000.db".to_string(),
            "transcript-5000.db".to_string(),
        ],
        "组内必须是这四件：{names:?}"
    );
}

// ── 钉②：加密变体五件成组、缺一不残留 ────────────────────────────────────

#[test]
fn encrypted_backup_keeps_the_full_set_and_leaves_no_plaintext() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let key = rc_secret::backup::BackupKey::generate().unwrap();
    {
        let root = Root::open(&data).unwrap();
        seed(&root, 0);
    }
    seed_core(&data, "agent-a", "加密组里的底线");

    let report = {
        let mut root = Root::open(&data).unwrap();
        hub::backup::backup_with_key(&mut root, &data, 1_000, Some(&key)).unwrap()
    };
    assert!(report.transcript.ends_with(".db.enc"));
    assert!(report.memory.ends_with(".db.enc"));
    assert!(
        report.ledger.ends_with(".db.enc"),
        "ledger 必须跟着组走加密变体：{:?}",
        report.ledger
    );
    assert_eq!(
        report.core.as_deref(),
        Some("core-1000.db.enc"),
        "core 的加密变体必须与组同名同族：{:?}",
        report.core
    );

    let backups = data.join("backups");
    let mut expect = BTreeMap::new();
    for name in [
        "transcript-1000.db.enc",
        "memory-1000.db.enc",
        "ledger-1000.db.enc",
        "core-1000.db.enc",
        "backup-1000.json",
    ] {
        let path = backups.join(name);
        assert!(path.is_file(), "加密组缺 {name}");
        expect.insert(name.to_string(), std::fs::metadata(&path).unwrap().len());
    }
    assert_eq!(dir_snapshot(&backups), expect, "加密组必须恰好五件");

    // 加密段的文件名跟着组走，且**没有明文** .db 残留。
    let doc = read_manifest(&backups.join("backup-1000.json"));
    assert_eq!(doc["ledger"]["file"], "ledger-1000.db.enc");
    assert_eq!(doc["core"]["file"], "core-1000.db.enc");
    for leftover in ["transcript-1000.db", "memory-1000.db", "ledger-1000.db", "core-1000.db"] {
        assert!(!backups.join(leftover).exists(), "明文 {leftover} 残留");
    }

    // 同一时间戳重跑：探测拒绝，旧组五件**一个字节都不动**（缺一不残留的另一面）。
    let before = dir_snapshot(&backups);
    let again = {
        let mut root = Root::open(&data).unwrap();
        hub::backup::backup_with_key(&mut root, &data, 1_000, Some(&key))
    };
    assert!(again.is_err(), "同时间戳的第二组必须被拒");
    assert_eq!(dir_snapshot(&backups), before, "被拒的那一次动了旧组");
}

// ── 钉③：restore round-trip ─────────────────────────────────────────────

#[test]
fn restore_round_trip_brings_ledger_and_core_back() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let scope = water::distill_scope("dev-a", "sess-a");
    {
        let root = Root::open(&data).unwrap();
        seed(&root, 0);
    }
    seed_core(&data, "agent-a", "W28 往返的底线");

    let manifest_path = backup_now(&data, 5000);
    let doc = read_manifest(&manifest_path);
    let expected_ledger_rows = doc["ledger"]["rows"].as_i64().unwrap();
    let expected_audits: i64 = {
        let conn = ledger_conn(&data.join("ledger.db"));
        conn.query_row("SELECT count(*) FROM audit_log", [], |r| r.get(0))
            .unwrap()
    };

    // 毁库：水位推过备份点、再插一条审计、core 文档盖到 v2、再注册第二个 agent。
    {
        let root = Root::open(&data).unwrap();
        water::advance(
            root.ledger(),
            "distill",
            &scope,
            &Cursor {
                at_ms: 9999,
                turn_uid: turn_uid(9),
            },
            9999,
        )
        .unwrap();
        rc_ledger::device::append_audit(root.ledger(), 9999, "test", "wreck", None, "毁库行")
            .unwrap();
    }
    {
        let store = CoreStore::open(&data).unwrap();
        let mut payload = CorePayload::empty("agent-a");
        payload.l0 = vec!["毁库之后的内容".to_string()];
        let version = store.save_doc("agent-a", &payload, "dev-a", 9_999).unwrap();
        assert_eq!(version, 2, "夹具的毁库必须把文档盖到 v2");
        store.register_agent("agent-b", 9_999).unwrap();
    }

    let mut root = Root::open(&data).unwrap();
    let checked = hub::restore::restore(&mut root, &manifest_path, 6_000).unwrap();
    assert_eq!(
        checked.ledger_rows, expected_ledger_rows,
        "校验读数必须如实回报恢复的账本行数"
    );
    assert_eq!(
        checked.core_agents,
        Some(1),
        "校验读数必须如实回报恢复的 core 注册数"
    );

    // 账本回到备份点：游标、审计行数。
    {
        let root = Root::open(&data).unwrap();
        assert_eq!(
            water::read(root.ledger(), "distill", &scope).unwrap(),
            Some(Cursor {
                at_ms: 1000,
                turn_uid: turn_uid(0),
            }),
            "恢复必须把蒸馏游标设回备份点"
        );
        let audits: i64 = root
            .ledger()
            .conn()
            .query_row("SELECT count(*) FROM audit_log", [], |r| r.get(0))
            .unwrap();
        // restore 自己会在账本里落一行 restore+restored 审计（既有语义，restore_cli 钉着），
        // 所以行数 = 备份点 + restore 的那一行；毁库插下的 wreck 行必须消失。
        assert_eq!(
            audits,
            expected_audits + 1,
            "毁库插下的审计行没有被丢掉"
        );
    }

    // core 回到备份点：v1 内容读回、第二个 agent 的注册行消失。
    {
        let store = CoreStore::open(&data).unwrap();
        let payload = store.load_doc("agent-a").unwrap().expect("agent-a 必须有文档");
        assert_eq!(
            payload.l0,
            vec!["W28 往返的底线".to_string()],
            "恢复后的 core 文档不是备份点那份"
        );
        assert_eq!(store.doc_version("agent-a").unwrap(), 1, "文档必须回到 v1");
        assert!(
            store.load_doc("agent-b").is_err(),
            "备份点之后的 agent-b 不该跟着恢复回来"
        );
    }
}

// ── 钉⑤：旧清单兼容与版本门 ─────────────────────────────────────────────

#[test]
fn a_manifest_without_ledger_segment_still_restores() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        seed(&root, 0);
    }
    seed_core(&data, "agent-a", "旧清单不碰 core");
    let manifest_path = backup_now(&data, 5000);

    // 把清单退回本工单之前的形状：删掉 ledger/core 两段。
    let mut doc = read_manifest(&manifest_path);
    let obj = doc.as_object_mut().unwrap();
    obj.remove("ledger");
    obj.remove("core");
    std::fs::write(&manifest_path, serde_json::to_vec(&doc).unwrap()).unwrap();

    // 毁一行账本：旧清单恢复后这行**必须在**（ledger 段缺席 ⇒ restore 不碰 ledger）。
    {
        let root = Root::open(&data).unwrap();
        rc_ledger::device::append_audit(root.ledger(), 9999, "test", "keep", None, "新清单不会留它")
            .unwrap();
    }
    let mut root = Root::open(&data).unwrap();
    let checked = hub::restore::restore(&mut root, &manifest_path, 6_000).unwrap();
    assert_eq!(checked.ledger_rows, 0, "旧清单没有 ledger 段，读数必须是零而不是编的");
    assert_eq!(checked.core_agents, None, "旧清单没有 core 段，读数必须是空");
    let audits: i64 = root
        .ledger()
        .conn()
        .query_row("SELECT count(*) FROM audit_log", [], |r| r.get(0))
        .unwrap();
    // seed 1 + 毁库 1，再加 restore 自己留的那一行审计（既有语义）= 3；
    // 若 ledger 被误整替，这行断言会少到 2。
    assert_eq!(audits, 3, "旧清单的恢复不许碰账本（毁库行必须还在）");
    let store = CoreStore::open(&data).unwrap();
    let payload = store.load_doc("agent-a").unwrap().expect("core 不该被动");
    assert_eq!(payload.l0, vec!["旧清单不碰 core".to_string()]);
}

#[test]
fn a_damaged_ledger_backup_is_refused_before_anything_is_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    {
        let root = Root::open(&data).unwrap();
        seed(&root, 0);
    }
    let manifest_path = backup_now(&data, 5000);
    let doc = read_manifest(&manifest_path);
    let ledger_name = doc["ledger"]["file"].as_str().unwrap().to_string();
    let backup_db = manifest_path.parent().unwrap().join(&ledger_name);

    // 坏法一：版本号不是当前账本版本 ⇒ 形状不可知，拒。（校验只读清单与备份文件，
    // 不开活库；"被拒的校验不动活库"的断言在坏法二那一块。）
    {
        let conn = rusqlite::Connection::open(&backup_db).unwrap();
        conn.pragma_update(None, "user_version", rc_store::ddl::LEDGER_VERSION - 1)
            .unwrap();
    }
    assert!(
        hub::restore::check(&manifest_path).is_err(),
        "版本不对的 ledger 备份必须被校验拒掉"
    );

    // 坏法二：版本对、内容动了（多插一行审计）⇒ 行数/摘要与清单对不上，拒。
    {
        let conn = rusqlite::Connection::open(&backup_db).unwrap();
        conn.pragma_update(None, "user_version", rc_store::ddl::LEDGER_VERSION)
            .unwrap();
        conn.execute(
            "INSERT INTO audit_log(at_ms, actor, action, detail) VALUES(1,'x','x','x')",
            [],
        )
        .unwrap();
    }
    {
        let root = Root::open(&data).unwrap();
        assert!(
            hub::restore::check(&manifest_path).is_err(),
            "内容与清单对不上的 ledger 备份必须被拒"
        );
        // 拒绝发生在任何覆盖之前：活库的账本原封。
        let audits: i64 = root
            .ledger()
            .conn()
            .query_row("SELECT count(*) FROM audit_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(audits, 1, "被拒的校验不该动活库");
    }
}
