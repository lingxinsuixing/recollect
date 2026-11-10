use rc_net::key::{verify_b64, DeviceKey, HubKey};

#[test]
fn hub_key_is_stable_across_loads_and_signature_verifies() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.key");
    let k1 = HubKey::load_or_create(&path).unwrap();
    let pub1 = k1.public_b64();
    let sig = k1.sign_b64(b"hello");
    assert!(verify_b64(&pub1, b"hello", &sig));
    let k2 = HubKey::load_or_create(&path).unwrap();
    assert_eq!(pub1, k2.public_b64(), "第二次加载必须读到同一把钥匙");
}

#[test]
fn hub_key_file_is_exactly_two_lines_and_holds_no_path_or_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.key");
    let _ = HubKey::load_or_create(&path).unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "格式变了：{text}");
    assert_eq!(lines[0], "recollect-key-v1");
    assert!(!text.contains(&dir.path().display().to_string()), "私钥文件里出现路径");
}

#[cfg(unix)]
#[test]
fn key_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.key");
    let _ = HubKey::load_or_create(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "私钥权限是 {mode:o}，不是 600");
}

#[test]
fn device_id_is_derived_from_pubkey_and_stable_across_save_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("device.key");
    let k1 = DeviceKey::generate().unwrap();
    let id1 = k1.device_id();
    assert!(id1.starts_with("dev-"), "{id1}");
    assert_eq!(id1.len(), 20, "dev- + 16 hex：{id1}");
    k1.save(&path).unwrap();
    let k2 = DeviceKey::load(&path).unwrap();
    assert_eq!(id1, k2.device_id(), "同一条私钥必须导出同一个设备 id");
    assert_eq!(k1.public_b64(), k2.public_b64());
    let sig = k2.sign_b64(b"m");
    assert!(verify_b64(&k1.public_b64(), b"m", &sig));
    // 中心侧配对照表走的是同一条导出规则，不是第二份截断实现。
    assert_eq!(rc_net::key::device_id_of(&k1.public_b64()).unwrap(), id1);
    assert_eq!(rc_net::key::device_id_of("not-base64"), None);
}

#[test]
fn malformed_key_file_is_rejected_not_silently_regenerated() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("hub.key");
    std::fs::write(&path, "recollect-key-v1\nnot-base64\n").unwrap();
    assert!(HubKey::load_or_create(&path).is_err(), "坏钥匙必须响亮失败");
}
