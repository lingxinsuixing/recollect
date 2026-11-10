// crates/hub/tests/key_archive_cli.rs
//! B5b：中心密钥的加密归档。导出把 `hub.key` 的原字节放进密文箱，导入把它写回去；
//! 归档里没有明文私钥，口令只从 stdin 来（非 TTY），覆盖两个方向都拒。
//! 第 6 条走真 doctor 二进制：`key_archive` 块报导出时刻与公钥指纹。
use rc_net::key::HubKey;
use rc_store::Root;
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

/// doctor 与 hub 同在一个 target 目录，但 `cargo test -p hub` 只构建本包的 bin。
/// 找不到邻居产物时就地 `cargo build -p doctor`（与 rc-store 的 trybuild 同一类嵌套调用）；
/// 完整闸的 `cargo build --workspace` 早已把它建好，这一支只在单独跑本包时生效。
fn doctor_bin() -> PathBuf {
    let exe = Path::new(bin())
        .parent()
        .unwrap()
        .join(format!("recollect-doctor{}", std::env::consts::EXE_SUFFIX));
    if !exe.is_file() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
        let status = Command::new(cargo)
            .args(["build", "-p", "doctor"])
            .status()
            .expect("cargo runs");
        assert!(status.success(), "doctor 产物缺失，按需构建也失败了");
    }
    exe
}

fn data_arg(data: &Path) -> String {
    data.to_str().unwrap().to_owned()
}

/// 通过管道喂口令（`passphrase` 原样写入；空切片 = 立即 EOF 的空 stdin）。
fn run_with_stdin(args: &[&str], passphrase: &[u8]) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hub binary");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(passphrase)
        .expect("stdin is writable");
    child.wait_with_output().expect("hub exits")
}

/// 一个带中心私钥的数据目录：返回目录与公钥（base64url），句柄在函数返回前全部释放。
fn fixture(dir: &Path) -> (PathBuf, String) {
    let data = dir.join("data");
    let public = {
        let root = Root::open(&data).unwrap();
        let key = HubKey::load_or_create(&data.join("hub.key")).unwrap();
        let public = key.public_b64();
        drop(root);
        public
    };
    (data, public)
}

fn json_of(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn export_args<'a>(data: &'a str, archive: &'a str) -> Vec<&'a str> {
    vec!["key", "export", "--out", archive, "--data-dir", data]
}

fn import_args<'a>(data: &'a str, archive: &'a str) -> Vec<&'a str> {
    vec!["key", "import", "--from", archive, "--data-dir", data]
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[test]
fn export_then_import_round_trips_the_public_key() {
    let dir = tempfile::tempdir().unwrap();
    let (data_a, public_a) = fixture(&dir.path().join("a"));
    let archive = dir.path().join("a.enc");
    let a_archive = archive.to_str().unwrap().to_owned();
    let a_data = data_arg(&data_a);

    // 空 stdin（非 TTY，EOF）先拒：口令只从管道来，空就是空，而且不该留下归档。
    let empty = run_with_stdin(&export_args(&a_data, &a_archive), b"");
    assert_eq!(empty.status.code(), Some(3), "空口令不是成功：{}", stderr_of(&empty));
    assert!(
        stderr_of(&empty).contains("passphrase is empty"),
        "空口令的类别串不见了：{}",
        stderr_of(&empty)
    );
    assert!(!archive.exists(), "空口令也写出了归档");

    let exported = run_with_stdin(&export_args(&a_data, &a_archive), b"correct horse battery\n");
    assert_eq!(
        exported.status.code(),
        Some(0),
        "导出必须成功：{}",
        stderr_of(&exported)
    );
    let doc = json_of(&exported);
    assert_eq!(doc["exported"], true);
    let fingerprint = doc["fingerprint"].as_str().unwrap().to_owned();
    assert_eq!(
        fingerprint,
        rc_net::key::device_id_of(&public_a).unwrap(),
        "导出的指纹必须是 hub.key 的公钥指纹"
    );

    // 第二个数据目录导入；stdin 用一个 CRLF 行尾，验"只去一个行尾"。
    let data_b = dir.path().join("b").join("data");
    let b_data = data_arg(&data_b);
    let imported = run_with_stdin(&import_args(&b_data, &a_archive), b"correct horse battery\r\n");
    assert_eq!(
        imported.status.code(),
        Some(0),
        "导入必须成功：{}",
        stderr_of(&imported)
    );
    let doc = json_of(&imported);
    assert_eq!(doc["imported"], true);
    assert_eq!(doc["fingerprint"], fingerprint, "两边 stdout 的指纹必须一致");
    let restored = HubKey::load_or_create(&data_b.join("hub.key")).unwrap();
    assert_eq!(restored.public_b64(), public_a, "导入后的公钥与导出侧不同");

    // 零路径：成功面只有两个布尔/指纹字段。
    let text = format!(
        "{}{}{}{}",
        String::from_utf8_lossy(&exported.stdout),
        stderr_of(&exported),
        String::from_utf8_lossy(&imported.stdout),
        stderr_of(&imported)
    );
    assert!(
        !text.contains(&dir.path().display().to_string()),
        "输出带了数据目录路径：{text}"
    );
    assert!(!text.contains("correct horse battery"), "输出回显了口令：{text}");
}

#[test]
fn a_wrong_passphrase_does_not_import() {
    let dir = tempfile::tempdir().unwrap();
    let (data_a, _) = fixture(&dir.path().join("a"));
    let archive = dir.path().join("a.enc");
    let exported = run_with_stdin(
        &export_args(&data_arg(&data_a), archive.to_str().unwrap()),
        b"right passphrase\n",
    );
    assert_eq!(exported.status.code(), Some(0), "{}", stderr_of(&exported));

    let data_b = dir.path().join("b");
    let wrong = run_with_stdin(
        &import_args(&data_arg(&data_b), archive.to_str().unwrap()),
        b"wrong passphrase\n",
    );
    assert_eq!(wrong.status.code(), Some(3), "错口令不是成功：{}", stderr_of(&wrong));
    let err = stderr_of(&wrong);
    assert!(
        err.contains("cannot be opened"),
        "错口令的类别串不见了：{err}"
    );
    assert!(
        !err.contains(&dir.path().display().to_string()),
        "失败串带了路径：{err}"
    );
    assert!(!data_b.join("hub.key").exists(), "错口令却写出了私钥");
}

#[test]
fn the_archive_holds_no_plaintext_private_key() {
    let dir = tempfile::tempdir().unwrap();
    let (data, _) = fixture(&dir.path().join("a"));
    let archive = dir.path().join("a.enc");
    let out = run_with_stdin(
        &export_args(&data_arg(&data), archive.to_str().unwrap()),
        b"archive passphrase\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));

    let bytes = std::fs::read(&archive).unwrap();
    let key = std::fs::read(data.join("hub.key")).unwrap();
    assert!(
        bytes.starts_with(rc_secret::MAGIC),
        "归档没有以密文箱 magic 开头：{:?}",
        &bytes[..bytes.len().min(8)]
    );
    let needle_hex = hex_lower(&key).into_bytes();
    assert!(
        !bytes.windows(key.len()).any(|w| w == key.as_slice()),
        "归档里出现了私钥原字节"
    );
    assert!(
        !bytes.windows(needle_hex.len()).any(|w| w == needle_hex.as_slice()),
        "归档里出现了私钥的十六进制串"
    );
}

#[test]
fn import_refuses_to_overwrite_an_existing_key() {
    let dir = tempfile::tempdir().unwrap();
    let (data_a, _) = fixture(&dir.path().join("a"));
    let archive = dir.path().join("a.enc");
    let out = run_with_stdin(
        &export_args(&data_arg(&data_a), archive.to_str().unwrap()),
        b"import passphrase\n",
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));

    let data_b = dir.path().join("b");
    let first = run_with_stdin(
        &import_args(&data_arg(&data_b), archive.to_str().unwrap()),
        b"import passphrase\n",
    );
    assert_eq!(first.status.code(), Some(0), "第一次导入必须成功：{}", stderr_of(&first));
    let key_path = data_b.join("hub.key");
    let before = std::fs::read(&key_path).unwrap();

    let second = run_with_stdin(
        &import_args(&data_arg(&data_b), archive.to_str().unwrap()),
        b"import passphrase\n",
    );
    assert_eq!(second.status.code(), Some(3), "覆盖已有私钥不是成功：{}", stderr_of(&second));
    assert!(
        stderr_of(&second).contains("already exists"),
        "拒绝覆盖的类别串不见了：{}",
        stderr_of(&second)
    );
    assert_eq!(
        std::fs::read(&key_path).unwrap(),
        before,
        "第二次导入改动了原私钥字节"
    );
}

#[test]
fn export_refuses_an_existing_archive() {
    let dir = tempfile::tempdir().unwrap();
    let (data, _) = fixture(&dir.path().join("a"));
    let archive = dir.path().join("a.enc");
    let out_path = archive.to_str().unwrap().to_owned();
    let data_path = data_arg(&data);

    let first = run_with_stdin(&export_args(&data_path, &out_path), b"export passphrase\n");
    assert_eq!(first.status.code(), Some(0), "{}", stderr_of(&first));
    let before = std::fs::read(&archive).unwrap();

    let second = run_with_stdin(&export_args(&data_path, &out_path), b"export passphrase\n");
    assert_eq!(second.status.code(), Some(3), "覆盖已有归档不是成功：{}", stderr_of(&second));
    assert!(
        stderr_of(&second).contains("already exists"),
        "拒绝覆盖的类别串不见了：{}",
        stderr_of(&second)
    );
    assert_eq!(
        std::fs::read(&archive).unwrap(),
        before,
        "第二次导出改动了原归档字节"
    );
}

#[test]
fn doctor_reports_the_archive_time_and_fingerprint() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data-canary-key-archive-9173");
    let data_path = data_arg(&data);
    // `snapshot publish` 生成 hub.key，并让 hub 落下公开指纹缓存；它不写导出计数。
    let published = Command::new(bin())
        .args(["snapshot", "publish", "--data-dir", &data_path])
        .output()
        .unwrap();
    assert_eq!(published.status.code(), Some(0), "{}", stderr_of(&published));
    let key = HubKey::load_or_create(&data.join("hub.key")).unwrap();
    let expected = rc_net::key::device_id_of(&key.public_b64()).unwrap();

    let read_doctor = |data: &str| -> Value {
        let out = Command::new(doctor_bin())
            .args(["--capabilities", "--json", "--data-dir", data])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(0), "doctor 失败：{}", stderr_of(&out));
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(!text.contains(data), "doctor 读数带了路径：{text}");
        serde_json::from_str(&text).expect("doctor stdout 必须是单个 JSON 对象")
    };

    // 还没导出过：时刻是 0，但密钥在 ⇒ 指纹读得出来（缓存来自生成私钥的那条命令）。
    let before = read_doctor(&data_path);
    assert_eq!(before["key_archive"]["last_at_ms"], 0, "未导出前的时刻不是 0：{before}");
    assert_eq!(
        before["key_archive"]["fingerprint"], expected,
        "未导出但私钥在时，指纹必须仍然读得出来：{before}"
    );

    let archive = dir.path().join("a.enc");
    let exported = run_with_stdin(
        &export_args(&data_path, archive.to_str().unwrap()),
        b"doctor passphrase\n",
    );
    assert_eq!(exported.status.code(), Some(0), "{}", stderr_of(&exported));
    let fingerprint = json_of(&exported)["fingerprint"].as_str().unwrap().to_owned();

    let after = read_doctor(&data_path);
    assert!(
        after["key_archive"]["last_at_ms"].as_i64().unwrap() > 0,
        "导出之后 last_at_ms 必须大于 0：{after}"
    );
    assert_eq!(
        after["key_archive"]["fingerprint"], fingerprint,
        "doctor 的指纹必须与导出的指纹一致：{after}"
    );
}
