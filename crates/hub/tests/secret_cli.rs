// crates/hub/tests/secret_cli.rs
//! B2a-2：`hub secret init/put/list/rm` 的唯一出口面。
//! CLI 是不是"值只经过不留下"只有真进程看得见：stdout/stderr 的字节、退出码、箱里的值。
use rc_secret::SecretsBox;
use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("hub binary")
}

/// 喂 stdin 的那一支：`.output()` 自带 null stdin，测试里需要真的把值送进去。
fn run_with_stdin(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hub binary");
    // 子进程可能在校验阶段就退出（坏 ref），此时写侧收到断管不是测试失败。
    let _ = child.stdin.take().expect("stdin pipe").write_all(input);
    child.wait_with_output().expect("hub output")
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn data_arg(data: &Path) -> String {
    data.to_str().unwrap().to_string()
}

/// `.pass` 去掉行尾换行之后的那 64 个字符——箱要拿它开。
fn pass_phrase(data: &Path, name: &str) -> Vec<u8> {
    let mut raw = std::fs::read(data.join(format!("{name}.pass"))).unwrap();
    if raw.last() == Some(&b'\n') {
        raw.pop();
        if raw.last() == Some(&b'\r') {
            raw.pop();
        }
    }
    raw
}

fn open_box(data: &Path, name: &str) -> SecretsBox {
    let phrase = pass_phrase(data, name);
    SecretsBox::open(&data.join(format!("{name}.enc")), &phrase).unwrap()
}

#[test]
fn init_creates_an_encrypted_box_and_a_private_pass_file() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let d = data_arg(&data);

    let out = run(&["secret", "init", "--data-dir", d.as_str()]);
    assert_eq!(out.status.code(), Some(0), "init 失败：{}", lossy(&out.stderr));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json, json!({ "created": "secrets" }));

    let enc = data.join("secrets.enc");
    let pass = data.join("secrets.pass");
    assert!(enc.is_file(), "箱文件不在");
    assert!(pass.is_file(), "口令文件不在");

    // 箱真的能用那段口令我打开，而且刚建出来是空的。
    let b = open_box(&data, "secrets");
    assert!(b.list().is_empty());
    drop(b);

    // Unix 才读得到 mode；Windows 的 ACL 由 rc-secret 自己的测试覆盖。
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&pass).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "口令文件对组/其他人可读：{mode:o}");
    }
}

#[test]
fn put_list_and_rm_round_trip_without_echoing_the_value() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let d = data_arg(&data);
    assert!(run(&["secret", "init", "--data-dir", d.as_str()]).status.success());

    let out = run_with_stdin(
        &["secret", "put", "secrets:openai", "--value-stdin", "--data-dir", d.as_str()],
        b"sk-live-TOPSECRET-9f00\n",
    );
    assert_eq!(out.status.code(), Some(0), "put 失败：{}", lossy(&out.stderr));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json, json!({ "stored": "secrets:openai" }));
    assert!(!lossy(&out.stdout).contains("TOPSECRET"), "值进了 stdout");
    assert!(!lossy(&out.stderr).contains("TOPSECRET"), "值进了 stderr");

    let out = run(&["secret", "list", "secrets", "--data-dir", d.as_str()]);
    assert_eq!(out.status.code(), Some(0), "list 失败：{}", lossy(&out.stderr));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json, json!({ "slots": ["openai"] }));
    assert!(!lossy(&out.stdout).contains("TOPSECRET"), "list 把值带出来了");

    // 真开箱：命令行回的是地址，值只在箱里。
    let b = open_box(&data, "secrets");
    assert_eq!(b.get("openai"), Some("sk-live-TOPSECRET-9f00"));
    drop(b);

    // 只掉一个尾换行：值内部的换行必须原样活着。
    let out = run_with_stdin(
        &["secret", "put", "secrets:multi", "--value-stdin", "--data-dir", d.as_str()],
        b"line1\nline2\n",
    );
    assert_eq!(out.status.code(), Some(0), "put 失败：{}", lossy(&out.stderr));
    let b = open_box(&data, "secrets");
    assert_eq!(b.get("multi"), Some("line1\nline2"));
    drop(b);

    let out = run(&["secret", "rm", "secrets:openai", "--data-dir", d.as_str()]);
    assert_eq!(out.status.code(), Some(0), "rm 失败：{}", lossy(&out.stderr));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json, json!({ "removed": "secrets:openai", "existed": true }));

    // 幂等：第二发还是 0，只是 existed 变 false。
    let out = run(&["secret", "rm", "secrets:openai", "--data-dir", d.as_str()]);
    assert_eq!(out.status.code(), Some(0), "缺格必须仍是 0：{}", lossy(&out.stderr));
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json, json!({ "removed": "secrets:openai", "existed": false }));

    let out = run(&["secret", "list", "secrets", "--data-dir", d.as_str()]);
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json, json!({ "slots": ["multi"] }));
}

#[test]
fn init_refuses_to_overwrite_an_existing_box() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let d = data_arg(&data);
    assert!(run(&["secret", "init", "--data-dir", d.as_str()]).status.success());

    let enc_before = std::fs::read(data.join("secrets.enc")).unwrap();
    let pass_before = std::fs::read(data.join("secrets.pass")).unwrap();

    let out = run(&["secret", "init", "--data-dir", d.as_str()]);
    assert_eq!(out.status.code(), Some(3), "第二次 init 必须拒绝");
    assert!(
        lossy(&out.stderr).contains("already exists"),
        "拒绝语必须点出 already exists：{}",
        lossy(&out.stderr)
    );
    assert_eq!(std::fs::read(data.join("secrets.enc")).unwrap(), enc_before, "箱被改了");
    assert_eq!(std::fs::read(data.join("secrets.pass")).unwrap(), pass_before, "口令文件被改了");
}

#[test]
fn a_missing_box_and_a_wrong_pass_phrase_fail_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let d = data_arg(&data);

    let out = run_with_stdin(
        &["secret", "put", "missing:slot", "--value-stdin", "--data-dir", d.as_str()],
        b"TOPSECRET-value\n",
    );
    assert_eq!(out.status.code(), Some(3), "缺箱必须响亮失败");
    assert!(
        lossy(&out.stderr).contains("secret box not found"),
        "缺箱的短语不对：{}",
        lossy(&out.stderr)
    );
    assert!(!lossy(&out.stdout).contains("TOPSECRET"), "失败路径把值打出来了");

    assert!(run(&["secret", "init", "--data-dir", d.as_str()]).status.success());
    // 把一个不是口令的串写进口令文件：箱因此打不开（AEAD 认证失败）。
    std::fs::write(data.join("secrets.pass"), b"not-the-passphrase\n").unwrap();
    let out = run_with_stdin(
        &["secret", "put", "secrets:slot", "--value-stdin", "--data-dir", d.as_str()],
        b"TOPSECRET-value\n",
    );
    assert_eq!(out.status.code(), Some(3), "错口令必须失败");
    assert!(
        lossy(&out.stderr).contains("secret box cannot be opened"),
        "错口令的短语不对：{}",
        lossy(&out.stderr)
    );
    assert!(!lossy(&out.stdout).contains("TOPSECRET"), "失败路径把值打出来了");
    assert!(!lossy(&out.stderr).contains("TOPSECRET"), "失败路径把值打出来了");
}

#[test]
fn bad_refs_and_empty_stdin_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let d = data_arg(&data);

    for bad in ["noslot", "box:slot:extra", "../evil:slot"] {
        let out = run_with_stdin(
            &["secret", "put", bad, "--value-stdin", "--data-dir", d.as_str()],
            b"value\n",
        );
        assert_eq!(out.status.code(), Some(3), "坏 ref `{bad}` 没被拒");
        let err = lossy(&out.stderr);
        assert!(err.contains("box:slot"), "坏 ref `{bad}` 的短语不对：{err}");
        assert!(!err.contains("../evil"), "坏 ref 被原样回显了：{err}");
    }

    let out = run(&["secret", "init", "--box", "bad:name", "--data-dir", d.as_str()]);
    assert_eq!(out.status.code(), Some(3), "含 `:` 的箱名不是文件名 stem");
    assert!(lossy(&out.stderr).contains("safe token"));

    assert!(run(&["secret", "init", "--data-dir", d.as_str()]).status.success());
    let out = run_with_stdin(
        &["secret", "put", "secrets:empty", "--value-stdin", "--data-dir", d.as_str()],
        b"",
    );
    assert_eq!(out.status.code(), Some(3), "空值必须被拒");
    assert!(
        lossy(&out.stderr).contains("secret value is empty"),
        "空值的短语不对：{}",
        lossy(&out.stderr)
    );

    // 不带 --value-stdin：连 stdin 都不读，直接按用法错误回。
    let out = run(&["secret", "put", "secrets:nope", "--data-dir", d.as_str()]);
    assert_eq!(out.status.code(), Some(3));
    assert!(
        lossy(&out.stderr).contains("--value-stdin"),
        "缺旗标的短语不对：{}",
        lossy(&out.stderr)
    );
}
