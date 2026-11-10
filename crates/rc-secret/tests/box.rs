use rc_secret::{read_secret, SecretError, SecretsBox};
use std::path::PathBuf;

const PASS: &[u8] = b"test-passphrase-123";

fn box_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("secrets.enc")
}

#[test]
fn round_trips_put_list_get_remove_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = box_path(&dir);
    {
        let mut b = SecretsBox::create(&path, PASS).unwrap();
        b.put("alpha", "secret-alpha").unwrap();
        b.put("beta", "secret-beta").unwrap();
        assert_eq!(b.list(), vec!["alpha", "beta"]);
        assert_eq!(b.get("alpha"), Some("secret-alpha"));
        assert!(b.remove("beta").unwrap());
        assert_eq!(b.list(), vec!["alpha"]);
    }
    {
        let b = SecretsBox::open(&path, PASS).unwrap();
        assert_eq!(b.get("alpha"), Some("secret-alpha"));
        assert_eq!(b.get("beta"), None);
    }
    assert!(matches!(
        SecretsBox::create(&path, PASS),
        Err(SecretError::AlreadyExists)
    ));
}

#[test]
fn a_wrong_passphrase_is_open_failed() {
    let dir = tempfile::tempdir().unwrap();
    let path = box_path(&dir);
    {
        let mut b = SecretsBox::create(&path, PASS).unwrap();
        b.put("alpha", "secret-alpha").unwrap();
    }
    assert!(matches!(
        SecretsBox::open(&path, b"wrong-passphrase"),
        Err(SecretError::OpenFailed)
    ));
}

#[test]
fn a_tampered_file_is_open_failed() {
    let dir = tempfile::tempdir().unwrap();
    let path = box_path(&dir);
    {
        let mut b = SecretsBox::create(&path, PASS).unwrap();
        b.put("alpha", "secret-alpha").unwrap();
    }
    let mut bytes = std::fs::read(&path).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    std::fs::write(&path, &bytes).unwrap();
    assert!(matches!(
        SecretsBox::open(&path, PASS),
        Err(SecretError::OpenFailed)
    ));
}

#[test]
fn the_file_holds_neither_the_secret_nor_the_passphrase() {
    let dir = tempfile::tempdir().unwrap();
    let path = box_path(&dir);
    let secret = b"CANARY-SECRET-9f2a";
    {
        let mut b = SecretsBox::create(&path, PASS).unwrap();
        b.put("alpha", std::str::from_utf8(secret).unwrap()).unwrap();
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        !bytes.windows(secret.len()).any(|w| w == secret),
        "文件里出现了明文"
    );
    assert!(
        !bytes.windows(PASS.len()).any(|w| w == PASS),
        "文件里出现了口令"
    );
}

#[test]
fn a_bad_slot_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = box_path(&dir);
    let mut b = SecretsBox::create(&path, PASS).unwrap();
    assert!(matches!(b.put("bad slot", "x"), Err(SecretError::BadSlot)));
    assert!(matches!(b.put("a/b", "x"), Err(SecretError::BadSlot)));
    assert!(b.get("bad slot").is_none());
}

#[test]
fn a_missing_slot_is_none() {
    let dir = tempfile::tempdir().unwrap();
    let path = box_path(&dir);
    let b = SecretsBox::create(&path, PASS).unwrap();
    assert!(b.get("nope").is_none());
    assert!(b.list().is_empty());
}

/// 读箱约定：`<data>/<box>.enc` + `<data>/<box>.pass`；口令文件带一个行尾换行。
#[test]
fn read_secret_round_trips_from_the_data_dir() {
    let dir = tempfile::tempdir().unwrap();
    {
        let mut b = SecretsBox::create(&box_path(&dir), PASS).unwrap();
        b.put("openai", "sk-x").unwrap();
    }
    std::fs::write(dir.path().join("secrets.pass"), b"test-passphrase-123\n").unwrap();
    assert_eq!(
        read_secret(dir.path(), "secrets", "openai").unwrap().as_str(),
        "sk-x"
    );
    assert!(matches!(
        read_secret(dir.path(), "secrets", "nope"),
        Err(SecretError::MissingSlot)
    ));
}

#[test]
fn read_secret_separates_missing_box_missing_pass_and_bad_box() {
    let dir = tempfile::tempdir().unwrap();
    // 空目录：`.enc` 先缺 ⇒ 箱还没建，不是"箱坏了"。
    assert!(matches!(
        read_secret(dir.path(), "secrets", "openai"),
        Err(SecretError::BoxMissing)
    ));
    {
        let mut b = SecretsBox::create(&box_path(&dir), PASS).unwrap();
        b.put("openai", "sk-x").unwrap();
    }
    // 只有 `.enc`：口令文件没落。
    assert!(matches!(
        read_secret(dir.path(), "secrets", "openai"),
        Err(SecretError::PassMissing)
    ));
    // 箱名是文件名 stem：带 `:` 的名字会把 `box:slot` 切错。
    assert!(matches!(
        read_secret(dir.path(), "bad:box", "x"),
        Err(SecretError::BadBox)
    ));
    assert!(matches!(
        read_secret(dir.path(), "secrets", "bad slot"),
        Err(SecretError::BadSlot)
    ));
}
