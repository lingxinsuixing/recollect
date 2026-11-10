use serde_json::Value;
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn pair_device(data: &Path) -> String {
    let mut root = rc_store::Root::open(data).unwrap();
    let dev = rc_net::key::DeviceKey::generate().unwrap();
    let (code, hash) = rc_net::pair::new_pair_code().unwrap();
    rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, 1, 400_000).unwrap();
    let (_token, token_hash) = rc_net::pair::new_token().unwrap();
    let outcome = rc_ledger::device::pair(
        root.ledger_mut(),
        &hash,
        &dev.device_id(),
        &dev.public_b64(),
        &token_hash,
        2,
    )
    .unwrap();
    assert_eq!(outcome, rc_ledger::device::PairOutcome::Paired, "code={code}");
    dev.device_id()
}

#[test]
fn pair_prints_a_code_and_no_path() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data-canary-xyz");
    let out = Command::new(bin())
        .arg("pair")
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let doc: Value = serde_json::from_str(text.trim()).unwrap();
    let code = doc["code"].as_str().unwrap();
    assert_eq!(code.len(), 11);
    assert_eq!(code.as_bytes()[5], b'-');
    assert!(doc["expires_at_ms"].as_i64().unwrap() > 0);
    assert!(!text.contains("data-canary-xyz"), "配对的输出带了路径：{text}");

    let out2 = Command::new(bin())
        .arg("pair")
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    let doc2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    assert_ne!(code, doc2["code"].as_str().unwrap(), "两次签发了同一个码");
}

#[test]
fn revoke_is_idempotent_and_reports_already() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let device_id = pair_device(&data);
    let first = Command::new(bin())
        .arg("revoke")
        .arg("--device-id")
        .arg(&device_id)
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let doc: Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(doc["revoked"], true);
    assert_eq!(doc["already"], false);

    let second = Command::new(bin())
        .arg("revoke")
        .arg("--device-id")
        .arg(&device_id)
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert!(second.status.success());
    let doc: Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(doc["already"], true);
}

#[test]
fn usage_lists_serve_pair_revoke() {
    let out = Command::new(bin()).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8(out.stderr).unwrap();
    for word in ["serve", "pair", "revoke"] {
        assert!(err.contains(word), "用法面没点名 {word}：{err}");
    }
}

#[test]
fn serve_without_config_fails_loudly_and_creates_no_key() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = Command::new(bin())
        .arg("serve")
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("--hub-config"), "没点名缺的是哪个旗标：{err}");
    assert!(!data.join("hub.key").exists(), "配置都没给就生成了私钥");
}
