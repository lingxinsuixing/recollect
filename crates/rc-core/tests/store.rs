//! `core.db` 注册、版本、隔离与访问日志（设计 §10.1/§10.6）。

use rc_core::{
    aad, agent_table_suffix, key_path, load_or_create_key, open, table_names, AgentRecord,
    CoreError, CorePayload, CoreStore, NONCE_LEN,
};
use rusqlite::Connection;
use std::path::Path;

/// `core_access_log` 一行的投影（clippy 的 `type_complexity` 只认有名字的元组）。
type AccessRow = (i64, String, String, Option<String>, String, Option<String>);

fn new_store() -> (tempfile::TempDir, CoreStore) {    let dir = tempfile::tempdir().unwrap();
    let store = CoreStore::open(dir.path()).unwrap();
    (dir, store)
}

fn db(dir: &Path) -> Connection {
    Connection::open(dir.join("core.db")).unwrap()
}

fn table_names_in(dir: &Path) -> Vec<String> {
    let conn = db(dir);
    let mut st = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    st.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn count_rows(dir: &Path, table: &str) -> i64 {
    db(dir)
        .query_row(&format!("SELECT COUNT(*) FROM \"{table}\""), [], |r| r.get(0))
        .unwrap()
}

fn raw_doc(dir: &Path, table: &str) -> (i64, [u8; NONCE_LEN], Vec<u8>) {
    let (version, nonce, ciphertext) = db(dir)
        .query_row(
            &format!(
                "SELECT doc_version, nonce, ciphertext FROM \"{table}\" WHERE singleton=1"
            ),
            [],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .unwrap();
    (version, nonce.as_slice().try_into().unwrap(), ciphertext)
}

fn registry_row(dir: &Path, agent_id: &str) -> (String, String, String, i64) {
    db(dir)
        .query_row(
            "SELECT table_name, rev_table_name, key_id, doc_version FROM core_agents \
             WHERE agent_id=?1",
            [agent_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

#[test]
fn register_creates_both_tables_and_is_idempotent() {
    let (dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    store.register_agent("agent-a", 0).unwrap();

    let (doc, rev) = table_names("agent-a");
    let names = table_names_in(dir.path());
    assert!(names.contains(&doc), "{names:?}");
    assert!(names.contains(&rev), "{names:?}");
    let journal_mode: String = db(dir.path())
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_lowercase(), "wal");
    for global in ["core_agents", "core_access_log", "core_injection_log"] {
        assert!(names.contains(&global.to_string()), "{names:?}");
    }
    // 治理小刀④（2026-11-07 core.db 治理分诊）：`core_sync_state` 是死表（全仓零读者
    // 零写者，多设备同步未立项），已从 DDL 删除——新库不再长出它。
    assert!(!names.contains(&"core_sync_state".to_string()), "{names:?}");

    let agents = store.list_agents().unwrap();
    assert_eq!(agents.len(), 1, "re-registering must stay idempotent");
    assert_eq!(agents[0].agent_id, "agent-a");
    assert_eq!(agents[0].doc_version, 0);

    let h = agent_table_suffix("agent-a");
    let (table_name, rev_table_name, key_id, version) = registry_row(dir.path(), "agent-a");
    assert_eq!(table_name, format!("core_doc_{h}"));
    assert_eq!(rev_table_name, format!("core_rev_{h}"));
    assert_eq!(key_id, format!("k-{h}"));
    assert_eq!(version, 0);
    assert!(dir.path().join("keys").join(format!("core_{h}.key")).is_file());

    // 两张表里都还没有行。
    assert_eq!(count_rows(dir.path(), &doc), 0);
    assert_eq!(count_rows(dir.path(), &rev), 0);
}

/// 治理小刀④的存量库边界：升级前建的库可能还带着 `core_sync_state` 孤表。
/// 孤表是**合法状态**：打开不报错、不写迁移、也不去动它——读写照常。
#[test]
fn a_legacy_db_with_an_orphan_sync_state_table_still_opens() {
    let dir = tempfile::tempdir().unwrap();
    // 造一份"旧库"：先用新形状开一次，再手工把死表加回去（模拟升级前的存量）。
    {
        let store = CoreStore::open(dir.path()).unwrap();
        store.register_agent("agent-a", 0).unwrap();
    }
    db(dir.path())
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS core_sync_state(
               agent_id TEXT NOT NULL, peer_device_id TEXT NOT NULL,
               last_version INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL,
               PRIMARY KEY(agent_id, peer_device_id)
             );",
        )
        .unwrap();
    assert!(table_names_in(dir.path()).contains(&"core_sync_state".to_string()));

    // 重开：孤表不挡路；注册、读写全部照常。
    let store = CoreStore::open(dir.path()).unwrap();
    store
        .save_doc("agent-a", &CorePayload::empty("agent-a"), "dev", 1)
        .unwrap();
    assert!(store.load_doc("agent-a").unwrap().is_some());
    // 不写迁移：孤表原样留在库里。
    assert!(table_names_in(dir.path()).contains(&"core_sync_state".to_string()));
}

#[test]
fn save_load_round_trips_and_bumps_versions() {
    let (dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    assert!(store.load_doc("agent-a").unwrap().is_none());

    let mut payload = CorePayload::empty("agent-a");
    payload.persona.core_text = "慢一点，但记住".to_string();
    payload.interaction.unanswered_user_messages = 2;
    payload.relationship.trust = 0.7;

    assert_eq!(store.save_doc("agent-a", &payload, "dev-1", 1_000).unwrap(), 1);
    let loaded = store.load_doc("agent-a").unwrap().unwrap();
    assert_eq!(loaded.agent_id, "agent-a");
    assert_eq!(loaded, {
        let mut expected = payload.clone();
        expected.agent_id = "agent-a".to_string();
        expected
    });

    assert_eq!(store.save_doc("agent-a", &payload, "dev-1", 2_000).unwrap(), 2);

    let agents = store.list_agents().unwrap();
    assert_eq!(agents[0].doc_version, 2);
    assert_eq!(agents[0].updated_at_ms, 2_000);

    let (doc, rev) = table_names("agent-a");
    assert_eq!(count_rows(dir.path(), &doc), 1, "singleton doc row");
    assert_eq!(count_rows(dir.path(), &rev), 2, "one revision per save");

    let ops: Vec<(i64, String, String)> = {
        let conn = db(dir.path());
        let mut st = conn
            .prepare(&format!(
                "SELECT version, section, op FROM \"{rev}\" ORDER BY version"
            ))
            .unwrap();
        st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        ops,
        vec![
            (1, "doc".to_string(), "ADD".to_string()),
            (2, "doc".to_string(), "UPDATE".to_string()),
        ]
    );
}

#[test]
fn saving_forces_the_payload_agent_id_to_the_caller() {
    let (_dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    let mut payload = CorePayload::empty("somebody-else");
    payload.persona.core_text = "mine".to_string();
    assert_eq!(store.save_doc("agent-a", &payload, "dev", 1).unwrap(), 1);
    let loaded = store.load_doc("agent-a").unwrap().unwrap();
    assert_eq!(loaded.agent_id, "agent-a");
    assert_eq!(loaded.persona.core_text, "mine");
}

#[test]
fn unregistered_agents_are_refused() {
    let (_dir, store) = new_store();
    assert!(matches!(
        store.load_doc("agent-a"),
        Err(CoreError::Unregistered)
    ));
    assert!(matches!(
        store.save_doc("agent-a", &CorePayload::empty("agent-a"), "dev", 1),
        Err(CoreError::Unregistered)
    ));
}

#[test]
fn two_agents_are_isolated() {
    let (dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    store.register_agent("agent-b", 0).unwrap();

    let mut pa = CorePayload::empty("agent-a");
    pa.persona.core_text = "A only".to_string();
    let mut pb = CorePayload::empty("agent-b");
    pb.persona.core_text = "B only".to_string();
    store.save_doc("agent-a", &pa, "dev", 1).unwrap();
    store.save_doc("agent-b", &pb, "dev", 2).unwrap();

    assert_eq!(
        store.load_doc("agent-a").unwrap().unwrap().persona.core_text,
        "A only"
    );
    assert_eq!(
        store.load_doc("agent-b").unwrap().unwrap().persona.core_text,
        "B only"
    );

    // 直接读库只看到密文。
    let (doc_b, _) = table_names("agent-b");
    let (version, nonce, ciphertext) = raw_doc(dir.path(), &doc_b);
    assert_eq!(version, 1);
    assert!(
        !ciphertext.windows(6).any(|w| w == b"B only"),
        "ciphertext must not carry plaintext"
    );

    // A 的 key 解 B 的密文 ⇒ AEAD 失败；B 的 key 才解得开。
    let key_a = load_or_create_key(dir.path(), "agent-a").unwrap();
    let key_b = load_or_create_key(dir.path(), "agent-b").unwrap();
    let aad_b = aad("agent-b", 1, "doc");
    assert!(matches!(
        open(&key_a, &aad_b, &nonce, &ciphertext),
        Err(CoreError::Crypto)
    ));
    let plain = open(&key_b, &aad_b, &nonce, &ciphertext).unwrap();
    assert!(String::from_utf8(plain).unwrap().contains("B only"));
}

#[test]
fn a_missing_key_is_reported_and_never_recreated_on_load_or_save() {
    let (dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    let mut payload = CorePayload::empty("agent-a");
    payload.persona.core_text = "keep me".to_string();
    assert_eq!(store.save_doc("agent-a", &payload, "dev", 1).unwrap(), 1);

    let path = key_path(dir.path(), "agent-a");
    assert!(path.is_file(), "registration creates the key");
    std::fs::remove_file(&path).unwrap();

    // 读：缺失 ⇒ `KeyMissing`，而不是静默换一把新 key 去解旧密文。
    assert!(matches!(store.load_doc("agent-a"), Err(CoreError::KeyMissing)));
    // 写：同样只加载已存在的 key。
    assert!(matches!(
        store.save_doc("agent-a", &payload, "dev", 2),
        Err(CoreError::KeyMissing)
    ));

    // 两条路径都没把 key 文件重新造出来。
    assert!(!path.exists(), "load/save must not recreate the key file");
    assert_eq!(
        std::fs::read_dir(dir.path().join("keys")).unwrap().count(),
        0,
        "no key file was written back"
    );

    // 数据没被改写：版本仍停在 1。
    assert_eq!(store.list_agents().unwrap()[0].doc_version, 1);

    // 注册路径仍然负责造 key。
    store.register_agent("agent-a", 0).unwrap();
    assert!(path.is_file(), "re-registering restores the key file");
}

#[test]
fn revoke_drops_tables_and_registration_but_keeps_the_key_file() {
    let (dir, store) = new_store();
    store.register_agent("agent-a", 0).unwrap();
    store.register_agent("agent-b", 0).unwrap();
    store
        .save_doc("agent-a", &CorePayload::empty("agent-a"), "dev", 1)
        .unwrap();

    store.revoke_agent("agent-a").unwrap();
    let (doc, rev) = table_names("agent-a");
    let names = table_names_in(dir.path());
    assert!(!names.contains(&doc), "{names:?}");
    assert!(!names.contains(&rev), "{names:?}");
    assert!(names.contains(&table_names("agent-b").0), "{names:?}");
    assert!(matches!(
        store.load_doc("agent-a"),
        Err(CoreError::Unregistered)
    ));
    assert_eq!(store.list_agents().unwrap().len(), 1);
    assert!(matches!(
        store.revoke_agent("agent-a"),
        Err(CoreError::NotFound)
    ));
    assert!(dir
        .path()
        .join("keys")
        .join(format!("core_{}.key", agent_table_suffix("agent-a")))
        .is_file());
}

#[test]
fn invalid_agent_ids_are_rejected_before_touching_the_database() {
    let (dir, store) = new_store();
    let long = "x".repeat(65);
    for bad in [
        String::new(),
        "bad/agent".to_string(),
        "bad\"agent".to_string(),
        long,
        "sp ace".to_string(),
        "bad\nagent".to_string(),
    ] {
        assert!(
            matches!(store.register_agent(&bad, 0), Err(CoreError::BadAgentId)),
            "register {bad:?}"
        );
        assert!(
            matches!(store.load_doc(&bad), Err(CoreError::BadAgentId)),
            "load {bad:?}"
        );
        assert!(
            matches!(
                store.save_doc(&bad, &CorePayload::empty(&bad), "dev", 1),
                Err(CoreError::BadAgentId)
            ),
            "save {bad:?}"
        );
    }
    let agents: Vec<AgentRecord> = store.list_agents().unwrap();
    assert!(agents.is_empty());
    assert_eq!(count_rows(dir.path(), "core_agents"), 0);
}

#[test]
fn access_log_rows_are_written_as_given() {
    let (dir, store) = new_store();
    store
        .log_access(
            "agent-a",
            "core_brief",
            Some("persona"),
            "denied",
            Some("agent_id parameter is not allowed"),
            42,
        )
        .unwrap();
    store
        .log_access("agent-a", "core_brief", None, "ok", None, 43)
        .unwrap();

    let conn = db(dir.path());
    let rows: Vec<AccessRow> = {
        let mut st = conn
            .prepare(
                "SELECT at_ms, caller_agent_id, tool, target_section, result, detail \
                 FROM core_access_log ORDER BY id",
            )
            .unwrap();
        st.query_map([], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    };
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].0, 42);
    assert_eq!(rows[0].1, "agent-a");
    assert_eq!(rows[0].2, "core_brief");
    assert_eq!(rows[0].3.as_deref(), Some("persona"));
    assert_eq!(rows[0].4, "denied");
    assert!(rows[0].5.as_deref().unwrap().contains("agent_id"));
    assert_eq!(rows[1].3, None);
    assert_eq!(rows[1].5, None);
}
