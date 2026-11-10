// crates/hub/tests/restore_extras.rs
//! F2：restore 的 extras（`llm.toml` / `*.enc` / `*.pass`）只有操作员显式 `--with-extras`
//! 才写回；清单必须绑在本组上（明文清单禁带 extras、`stored` 必须是本组规范名、
//! `source` 不许指向 `hub.key`/`FENCED`/`REQUIRE_EPOCH`）。
//!
//! 清单是未签名的普通 JSON：能写备份目录的人可以伪造它，所以"校验通过"不等于"该写"。
use rc_secret::backup::BackupKey;
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 备份点上的配置（恢复成功的判据是逐字节等于它）。
const BACKUP_CONFIG: &[u8] = b"[seat]\nprovider = \"qwen\"\nsecret_ref = \"qwen:key\"\n";
/// 恢复目标在备份之后被改成的样子（不带开关时必须原样保留）。
const OPERATOR_CONFIG: &[u8] = b"[seat]\nprovider = \"local\"\n";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hub binary")
}

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}

fn read_manifest(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

fn write_manifest(path: &Path, doc: &Value) {
    std::fs::write(path, serde_json::to_vec(doc).unwrap()).unwrap();
}

/// 一份加密备份：数据目录里有 `llm.toml`，随组走成一枚 extra。
struct Encrypted {
    data: PathBuf,
    manifest: PathBuf,
    key_path: PathBuf,
    key: BackupKey,
}

fn encrypted_backup(dir: &Path) -> Encrypted {
    let data = dir.join("center");
    {
        let _root = Root::open(&data).unwrap();
    }
    std::fs::write(data.join("llm.toml"), BACKUP_CONFIG).unwrap();
    let key = BackupKey::generate().unwrap();
    let key_path = dir.join("backup.key");
    key.save(&key_path).unwrap();
    let out = run(&[
        "backup",
        "--data-dir",
        data.to_str().unwrap(),
        "--backup-key-file",
        key_path.to_str().unwrap(),
        "--now-ms",
        "1000",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "夹具备份必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = data.join("backups").join("backup-1000.json");
    assert!(
        manifest.is_file(),
        "夹具备份没落下清单：{}",
        manifest.display()
    );
    Encrypted {
        data,
        manifest,
        key_path,
        key,
    }
}

fn restore_args<'a>(fx: &'a Encrypted, data: &'a str, extra: &[&'a str]) -> Vec<&'a str> {
    let mut args = vec![
        "restore",
        "--manifest",
        fx.manifest.to_str().unwrap(),
        "--backup-key-file",
        fx.key_path.to_str().unwrap(),
        "--data-dir",
        data,
        "--confirm",
    ];
    args.extend_from_slice(extra);
    args
}

// ── ① 绑组名：`stored` 不是本组规范名 ⇒ 拒绝 ──────────────────────────────

#[test]
fn a_tampered_extra_name_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let fx = encrypted_backup(dir.path());
    let backups = fx.data.join("backups");
    // 复制出**第二份真实密文**再把清单指过去：旧代码只查"纯文件名 + 解密 + sha/字节"，
    // 这条伪造会一路过校验（攻击者因此能让恢复读目录里任意一份可解密文件）。
    std::fs::copy(
        backups.join("extra-1000-llm.toml.enc"),
        backups.join("extra-1-llm.toml.enc"),
    )
    .unwrap();
    let mut doc = read_manifest(&fx.manifest);
    doc["extras"][0]["stored"] = json!("extra-1-llm.toml.enc");
    write_manifest(&fx.manifest, &doc);

    let dd = fx.data.to_str().unwrap().to_string();
    let out = run(&restore_args(&fx, &dd, &[]));
    assert_eq!(
        out.status.code(),
        Some(3),
        "非规范 stored 名必须被拒：stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("the backup does not check out"), "{text}");
}

// ── ② 明文清单禁带 extras ─────────────────────────────────────────────────

#[test]
fn a_plaintext_manifest_may_not_carry_extras() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("center");
    {
        let _root = Root::open(&data).unwrap();
    }
    let out = run(&[
        "backup",
        "--data-dir",
        data.to_str().unwrap(),
        "--now-ms",
        "1000",
    ]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "夹具备份必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let manifest = data.join("backups").join("backup-1000.json");
    // 伪造：往备份目录丢一份"配置"，再让清单以明文 extras 的形式登记它。
    let forged: &[u8] = b"[seat]\nprovider = \"attacker\"\n";
    std::fs::write(data.join("backups").join("llm.toml"), forged).unwrap();
    let mut doc = read_manifest(&manifest);
    doc["extras"] = json!([{
        "source": "llm.toml",
        "stored": "llm.toml",
        "sha256": rc_net::protocol::sha256_hex(forged),
        "bytes": forged.len(),
    }]);
    write_manifest(&manifest, &doc);

    let dd = data.to_str().unwrap();
    let out = run(&[
        "restore",
        "--manifest",
        manifest.to_str().unwrap(),
        "--data-dir",
        dd,
        "--confirm",
    ]);
    assert_eq!(
        out.status.code(),
        Some(3),
        "明文清单带 extras 必须被拒：stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("the backup does not check out"), "{text}");
    assert!(
        !data.join("llm.toml").exists(),
        "被拒的恢复不该写任何 extra"
    );
}

// ── ③ 不带 --with-extras：extras 只校验不写 ───────────────────────────────

#[test]
fn extras_are_checked_but_not_written_without_the_switch() {
    let dir = tempfile::tempdir().unwrap();
    let fx = encrypted_backup(dir.path());
    // 备份之后操作员改了配置：不带开关的恢复必须原样保留它。
    std::fs::write(fx.data.join("llm.toml"), OPERATOR_CONFIG).unwrap();
    let dd = fx.data.to_str().unwrap().to_string();
    let out = run(&restore_args(&fx, &dd, &[]));
    assert_eq!(
        out.status.code(),
        Some(0),
        "带 --confirm 但不带 --with-extras 必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["restored"], true);
    assert_eq!(doc["extras"], 1, "extras 仍是校验通过的份数");
    assert_eq!(doc["extras_restored"], 0);
    assert_eq!(doc["extras_skipped"], 1);
    assert_eq!(
        std::fs::read(fx.data.join("llm.toml")).unwrap(),
        OPERATOR_CONFIG,
        "没带 --with-extras 却写回了 extras"
    );
}

// ── ④ 带 --with-extras：按备份内容写回 ────────────────────────────────────

#[test]
fn extras_are_written_with_the_switch() {
    let dir = tempfile::tempdir().unwrap();
    let fx = encrypted_backup(dir.path());
    std::fs::write(fx.data.join("llm.toml"), OPERATOR_CONFIG).unwrap();
    let dd = fx.data.to_str().unwrap().to_string();
    let out = run(&restore_args(&fx, &dd, &["--with-extras"]));
    assert_eq!(
        out.status.code(),
        Some(0),
        "带 --with-extras 的恢复必须成功：{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let doc = stdout_json(&out);
    assert_eq!(doc["restored"], true);
    assert_eq!(doc["extras"], 1);
    assert_eq!(doc["extras_restored"], 1);
    assert_eq!(doc["extras_skipped"], 0);
    assert_eq!(
        std::fs::read(fx.data.join("llm.toml")).unwrap(),
        BACKUP_CONFIG,
        "带 --with-extras 时 llm.toml 必须恢复成备份内容"
    );
}

// ── ⑤ 伪造 source：hub.key / FENCED / REQUIRE_EPOCH ───────────────────────

/// 清单里的 source 即使密文与 sha/字节都自洽（攻击者自己加密一份），
/// 也不许指向恢复目标里的钥匙与门禁文件。**校验阶段**就要拒——不是等写到一半才拒。
#[test]
fn forged_extras_may_not_target_keys_or_fences() {
    let dir = tempfile::tempdir().unwrap();
    let fx = encrypted_backup(dir.path());
    let backups = fx.data.join("backups");
    let attacker: &[u8] = b"attacker-supplied key material";
    for source in ["hub.key", "FENCED", "REQUIRE_EPOCH"] {
        let plain = dir.path().join(format!("{source}.forged"));
        std::fs::write(&plain, attacker).unwrap();
        let stored = hub::backup::extra_stored_name(1000, source);
        rc_secret::backup::encrypt_file(&plain, &backups.join(&stored), &fx.key).unwrap();

        let mut doc = read_manifest(&fx.manifest);
        doc["extras"] = json!([{
            "source": source,
            "stored": stored,
            "sha256": rc_net::protocol::sha256_hex(attacker),
            "bytes": attacker.len(),
        }]);
        write_manifest(&fx.manifest, &doc);

        let err = hub::restore::check_with_key(&fx.manifest, Some(&fx.key))
            .expect_err("伪造 source 必须在校验阶段被拒");
        assert!(
            err.to_string().contains("unsafe restore name"),
            "{source}: {err}"
        );
    }
}
