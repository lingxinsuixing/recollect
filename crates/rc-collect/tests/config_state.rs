use rc_collect::config::Config;
use rc_collect::state::StateDoc;

#[test]
fn config_rejects_unknown_field_and_bad_argv() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("collect.toml");
    std::fs::write(
        &p,
        r#"
[limits]
quiet_ms = 600000
[[source]]
name = "qoder"
kind = "files"
root = "C:/tmp"
glob = "*.jsonl"
converter = []
device_id = "local-qoder"
source_agent = "qoder"
"#,
    )
    .unwrap();
    let err = Config::load(&p).unwrap_err();
    assert!(err.contains("converter"), "{err}");
}

#[test]
fn state_roundtrip_is_atomic_and_keeps_none_as_null() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("collect-state.json");
    let mut s = StateDoc::default();
    s.files.insert(
        "C:/a.jsonl".into(),
        rc_collect::state::FileCursor {
            mtime_ms: 1,
            size: 2,
            offset: 2,
            last_turns: 0,
            head_sha256: "x".into(),
        },
    );
    s.sqlite.insert(
        "mcode".into(),
        rc_collect::state::SqliteCursor {
            last_created_at_ms: 5,
            last_msg_id: None,
        },
    );
    s.save(&p).unwrap();
    let back = StateDoc::load(&p).unwrap();
    assert_eq!(back.files["C:/a.jsonl"].offset, 2);
    assert!(back.sqlite["mcode"].last_msg_id.is_none());
}

#[test]
fn missing_config_is_loud_but_missing_state_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("nope.toml");
    assert!(Config::load(&cfg).is_err());
    let st = dir.path().join("nope.json");
    let doc = StateDoc::load(&st).unwrap();
    assert!(doc.files.is_empty() && doc.sqlite.is_empty());
}

#[test]
fn config_applies_defaults_and_rejects_bad_shapes() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("collect.toml");
    let root = dir.path().join("src");
    std::fs::create_dir_all(&root).unwrap();
    let ok = format!(
        r#"
[[source]]
name = "qoder"
kind = "files"
root = "{}"
glob = "*/*.jsonl"
converter = ["true"]
device_id = "dev-1"
source_agent = "qoder"
"#,
        root.to_string_lossy().replace('\\', "/")
    );
    std::fs::write(&p, &ok).unwrap();
    let cfg = Config::load(&p).unwrap();
    assert_eq!(cfg.limits.quiet_ms, 600_000);
    assert_eq!(cfg.limits.max_files_per_run, 3);
    assert_eq!(cfg.limits.max_turns_per_day, 300);
    assert_eq!(cfg.sources[0].project, "session");

    // 同 (device_id, source_agent) 不能出现在两个源里
    let dup = ok.clone()
        + &ok
            .clone()
            .replace("name = \"qoder\"", "name = \"qoder-dup\"");
    std::fs::write(&p, dup).unwrap();
    let err = Config::load(&p).unwrap_err();
    assert!(err.contains("device_id"), "{err}");

    // sqlite 源必须带 db/cursor_sql
    std::fs::write(
        &p,
        r#"
[[source]]
name = "mcode"
kind = "sqlite"
converter = ["true"]
device_id = "dev-2"
source_agent = "mcode"
"#,
    )
    .unwrap();
    let err = Config::load(&p).unwrap_err();
    assert!(err.contains("cursor_sql") || err.contains("db"), "{err}");

    // project 只允许 session 或 fixed:<safe_token>
    std::fs::write(
        &p,
        format!(
            r#"
[[source]]
name = "qoder"
kind = "files"
root = "{}"
glob = "*.jsonl"
converter = ["true"]
device_id = "dev-3"
source_agent = "qoder"
project = "random"
"#,
            root.to_string_lossy().replace('\\', "/")
        ),
    )
    .unwrap();
    let err = Config::load(&p).unwrap_err();
    assert!(err.contains("project"), "{err}");
}

#[test]
fn corrupted_state_is_an_error_not_empty() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("collect-state.json");
    std::fs::write(&p, "{not json").unwrap();
    assert!(StateDoc::load(&p).is_err());
}
