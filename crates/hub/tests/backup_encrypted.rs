//! 加密备份：独立 key 文件、只落 `.db.enc`、restore 用同一 key 解密；错/缺 key 拒绝。

use rc_memory::writers_x::put_card;
use rc_memory::{AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_secret::backup::BackupKey;
use rc_store::Root;
use serde_json::Value;
use std::process::Command;

fn synth(root: &mut Root) {
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO turns(turn_uid, session_key, device_id, role, body, body_fp,
                               turn_key_tier, project_hint, created_at_ms, ingested_at_ms,
                               commit_seq)
             VALUES('u-1','s-1','dev-1','human','加密备份正文','fp',1,NULL,100,100,0)",
            [],
        )
        .unwrap();
    root.transcript()
        .conn()
        .execute(
            "INSERT INTO sessions(session_key, device_id, source_agent, machine, project_hint,
                                  last_saved_at_ms)
             VALUES('s-1','dev-1','agent',NULL,NULL,100)",
            [],
        )
        .unwrap();
    put_card(
        root.memory(),
        &FactContent {
            kind: "note".into(),
            title: "加密备份卡".into(),
            body: "加密备份卡正文".into(),
            scope: Scope {
                kind: ScopeKind::Project,
                key: "proj".into(),
            },
            valid_to_ms: None,
        },
        &[Provenance {
            turn_uid: "u-1".into(),
            anchor_role: AnchorRole::Human,
        }],
        100,
    )
    .unwrap();
}

#[test]
fn encrypted_backup_restores_with_the_same_key() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("center");
    let key_path = dir.path().join("backup.key");
    let key = BackupKey::generate().unwrap();
    key.save(&key_path).unwrap();
    {
        let mut root = Root::open(&data).unwrap();
        synth(&mut root);
    }
    let report = {
        let mut root = Root::open(&data).unwrap();
        hub::backup::backup_with_key(&mut root, &data, 1_000, Some(&key)).unwrap()
    };
    assert!(report.transcript.ends_with(".db.enc"));
    assert!(report.memory.ends_with(".db.enc"));
    let backups = data.join("backups");
    assert!(backups.join(&report.transcript).is_file());
    assert!(!backups.join("transcript-1000.db").exists(), "明文转录备份残留");
    assert!(!backups.join("memory-1000.db").exists(), "明文记忆备份残留");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(backups.join("backup-1000.json")).unwrap()).unwrap();
    assert_eq!(manifest["encryption"]["key_id"], key.key_id());
    assert!(!manifest.to_string().contains(&key.key_id().repeat(3)));

    let restored = dir.path().join("restored");
    let mut target = Root::open(&restored).unwrap();
    let checked =
        hub::restore::check_with_key(&backups.join("backup-1000.json"), Some(&key)).unwrap();
    assert_eq!(checked.turns, 1);
    hub::restore::restore_with_key(&mut target, &backups.join("backup-1000.json"), 2_000, Some(&key))
        .unwrap();
    let turns: i64 = target
        .transcript()
        .conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0))
        .unwrap();
    let cards: i64 = target
        .memory()
        .conn()
        .query_row("SELECT count(*) FROM card", [], |r| r.get(0))
        .unwrap();
    assert_eq!((turns, cards), (1, 1));
}

#[test]
fn encrypted_backup_rejects_missing_or_wrong_key() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("center");
    let key = BackupKey::generate().unwrap();
    {
        let mut root = Root::open(&data).unwrap();
        synth(&mut root);
    }
    {
        let mut root = Root::open(&data).unwrap();
        hub::backup::backup_with_key(&mut root, &data, 1_000, Some(&key)).unwrap();
    }
    let manifest = data.join("backups").join("backup-1000.json");
    assert!(hub::restore::check_with_key(&manifest, None).is_err());

    let other = BackupKey::generate().unwrap();
    assert!(hub::restore::check_with_key(&manifest, Some(&other)).is_err());

    let mut target = Root::open(&dir.path().join("restored")).unwrap();
    assert!(hub::restore::restore_with_key(&mut target, &manifest, 2_000, Some(&other)).is_err());
}

#[test]
fn encrypted_backup_and_restore_cli_wire_up() {
    let dir = tempfile::tempdir().unwrap();
    let center = dir.path().join("center");
    {
        let mut root = Root::open(&center).unwrap();
        synth(&mut root);
    }
    let key_path = dir.path().join("backup.key");
    let binary = env!("CARGO_BIN_EXE_recollect-hub");
    let out = Command::new(binary)
        .args([
            "backup-key",
            "generate",
            "--output",
            key_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["created"], true);
    let key_id = doc["key_id"].as_str().unwrap().to_owned();

    let out = Command::new(binary)
        .args([
            "backup",
            "--data-dir",
            center.to_str().unwrap(),
            "--backup-key-file",
            key_path.to_str().unwrap(),
            "--now-ms",
            "1000",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(doc["name"].as_str().unwrap().ends_with(".enc"));
    assert!(doc["memory"].as_str().unwrap().ends_with(".enc"));

    let manifest = center.join("backups").join("backup-1000.json");
    let stored: Value = serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
    assert_eq!(stored["encryption"]["key_id"], key_id);

    let restored = dir.path().join("restored");
    let out = Command::new(binary)
        .args([
            "restore",
            "--manifest",
            manifest.to_str().unwrap(),
            "--confirm",
            "--data-dir",
            restored.to_str().unwrap(),
            "--backup-key-file",
            key_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["restored"], true);
}

/// 同一套系统、同一个人：LLM 配置和密钥箱跟着加密备份走，恢复一步到位；
/// 明文备份一个都不带。
#[test]
fn encrypted_backup_carries_llm_config_and_secret_box() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("center");
    let key = BackupKey::generate().unwrap();
    {
        let mut root = Root::open(&data).unwrap();
        synth(&mut root);
    }
    let config = data.join("llm.toml");
    std::fs::write(&config, b"[seat]\nprovider = \"qwen\"\nsecret_ref = \"openai:key\"\n").unwrap();
    std::fs::write(data.join("openai.enc"), b"box-bytes").unwrap();
    std::fs::write(data.join("openai.pass"), b"phrase\n").unwrap();

    {
        let mut root = Root::open(&data).unwrap();
        let report =
            hub::backup::backup_with_key(&mut root, &data, 1_000, Some(&key)).unwrap();
        assert_eq!(report.extras, 3, "llm.toml + .enc + .pass");
    }
    let backups = data.join("backups");
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(backups.join("backup-1000.json")).unwrap()).unwrap();
    let extras = manifest["extras"].as_array().unwrap();
    assert_eq!(extras.len(), 3, "{manifest}");
    let sources: Vec<&str> = extras
        .iter()
        .map(|e| e["source"].as_str().unwrap())
        .collect();
    assert_eq!(sources, vec!["llm.toml", "openai.enc", "openai.pass"]);
    for e in extras {
        assert!(e["stored"].as_str().unwrap().ends_with(".enc"));
    }
    // 配置与口令的明文不许出现在备份目录里。
    for forbidden in ["llm.toml", "openai.enc", "openai.pass"] {
        assert!(!backups.join(forbidden).exists(), "{forbidden} 明文残留");
    }

    let checked =
        hub::restore::check_with_key(&backups.join("backup-1000.json"), Some(&key)).unwrap();
    assert_eq!(checked.extras, 3);
    let restored = dir.path().join("restored");
    let mut target = Root::open(&restored).unwrap();
    hub::restore::restore_with_key(
        &mut target,
        &backups.join("backup-1000.json"),
        2_000,
        Some(&key),
    )
    .unwrap();
    assert_eq!(std::fs::read(restored.join("llm.toml")).unwrap(), std::fs::read(&config).unwrap());
    assert_eq!(std::fs::read(restored.join("openai.enc")).unwrap(), b"box-bytes");
    assert_eq!(std::fs::read(restored.join("openai.pass")).unwrap(), b"phrase\n");

    // 明文备份：同一份数据，extras 为空，且不产生任何 extra-* 文件。
    {
        let mut root = Root::open(&data).unwrap();
        let report = hub::backup::backup(&mut root, &data, 3_000).unwrap();
        assert_eq!(report.extras, 0, "明文备份不许带配置/密钥");
    }
    let plain: Value =
        serde_json::from_slice(&std::fs::read(backups.join("backup-3000.json")).unwrap()).unwrap();
    assert_eq!(plain["extras"].as_array().unwrap().len(), 0);
    let leftovers: Vec<String> = std::fs::read_dir(&backups)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.starts_with("extra-3000-"))
        .collect();
    assert!(leftovers.is_empty(), "明文组落了 extras：{leftovers:?}");
}

#[test]
fn retention_removes_extras_with_their_set() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("center");
    let key = BackupKey::generate().unwrap();
    {
        let mut root = Root::open(&data).unwrap();
        synth(&mut root);
    }
    std::fs::write(data.join("llm.toml"), b"[seat]\nprovider = \"qwen\"\n").unwrap();
    std::fs::write(data.join("openai.enc"), b"box").unwrap();
    std::fs::write(data.join("openai.pass"), b"pass\n").unwrap();
    for now_ms in 1..=8 {
        let mut root = Root::open(&data).unwrap();
        hub::backup::backup_with_key(&mut root, &data, now_ms, Some(&key)).unwrap();
    }
    let names: Vec<String> = std::fs::read_dir(data.join("backups"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        names.iter().filter(|n| n.starts_with("backup-")).count() == 7,
        "{names:?}"
    );
    assert!(
        !names.iter().any(|n| n.starts_with("extra-1-")),
        "最旧一组的 extras 必须跟着删：{names:?}"
    );
    assert!(
        names.iter().any(|n| n.starts_with("extra-8-")),
        "最新一组的 extras 必须在：{names:?}"
    );
}
