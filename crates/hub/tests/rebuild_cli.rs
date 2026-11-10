use std::process::Command;

// bin 句柄沿用 `import_cli.rs:4` 的真实写法（`CARGO_BIN_EXE_…`），不是 `cargo_bin` crate。
fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run_hub(args: &[&str]) -> std::process::Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

#[test]
fn index_rebuild_打印两段读数且零正文() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    // 夹具：本用例只做形状断言——空目录下 `Root::open` 会自建三个库，`rebuild` 对空记忆库
    // 返回 `rows_indexed = 0`，这已经足够验证"两段读数 + 零正文 + 退出码 0"这条契约。
    // （真实数据链路在 G6① 档位跑，本档不假装它已经在了。）
    // W38a 刀 A：报告扩为两段读数——memory 段 `rows_indexed` + 转录段 `turns_indexed`。
    let out = run_hub(&["index", "rebuild", "--data-dir", data.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).expect("stdout 必须是单个 JSON 对象");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["norm_version"], 1);
    assert!(v["shadow_tables"].is_number(), "{v}");
    assert!(v["rows_indexed"].is_number(), "{v}");
    assert!(v["turns_indexed"].is_number(), "转录段读数缺席：{v}");
    // 守卫 12：两段读数之外一个字节都不许多。
    assert_eq!(v.as_object().unwrap().len(), 5, "输出多出字段：{v}");
    assert!(!text.contains("CANARY-BODY-8842"), "输出含正文：{text}");
    assert!(!text.contains(&data.display().to_string()), "输出含路径：{text}");
    // Windows 上 JSON 把 `\` 转义成 `\\`，整串就不再是子串；临时目录那个组件名不含分隔符，再断一次。
    let tmp_component = data.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(!text.contains(&tmp_component), "输出含临时目录组件：{text}");
}

#[test]
fn index_status_在未建库上返回_null_读数且退出码为_0() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let out = run_hub(&["index", "status", "--data-dir", data.to_str().unwrap()]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["rebuilt_at_ms"], serde_json::Value::Null);
}

#[test]
fn 数据目录不可用时退出码为_3() {
    // Windows 与 Unix 都要能红：拿"某个已存在的普通文件当目录用"是最稳的不可用目录，
    // 两个平台上 `create_dir_all` 都会失败（ENOTDIR），而 `/nonexistent-…` 在 Windows 上
    // 走的是另一条错误分支，可能在某些权限配置下反而建得出来。
    let dir = tempfile::tempdir().unwrap();
    let not_a_dir = dir.path().join("file.txt");
    std::fs::write(&not_a_dir, b"x").unwrap();
    let out = run_hub(&["index", "rebuild", "--data-dir", not_a_dir.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(3), "运维失败必须是 3");
    // stderr 只有类别名，不许把那个文件路径回显出来（零路径规则）。
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(!err.contains(&not_a_dir.display().to_string()), "stderr 泄露路径：{err}");
    assert!(out.stdout.is_empty(), "运维失败不印报告：{out:?}");
}

/// 裁定 R19：`hub index … | head -1` 之后下游管道已经关了，裸 `println!` 会当场 panic
/// 把退出码变成 101——而作业其实成功了。这里用"spawn 之后立刻关掉 stdout 读端"复现那个形状。
/// 关掉读端一定发生在子进程打印之前（它还要开库、建表、写 marker，是毫秒级；这里是微秒级），
/// 所以这条不是碰运气：把 `write_line` 改回 `println!` 就红（见报告的证伪记录）。
#[test]
fn 输出接提前关闭的管道时退出码仍是_0() {
    use std::process::{Command, Stdio};
    let dir = tempfile::tempdir().unwrap();
    let fresh = dir.path().join("fresh");
    let built = dir.path().join("built");
    let fresh_s = fresh.to_str().unwrap();
    let built_s = built.to_str().unwrap();

    // 先各跑一次**不关管道**的正常调用，确认作业本身成功——
    // 否则"退出码 0"可能来自作业提前失败走了另一条路，那条断言就是恒真的。
    // 三组覆盖 run_index 的三个打印点：status 的 None 臂、rebuild 那一行、status 的 Some 臂。
    for (job, d) in [("status", fresh_s), ("rebuild", built_s), ("status", built_s)] {
        let out = run_hub(&["index", job, "--data-dir", d]);
        assert!(out.status.success(), "{job} 本身就没成功，管道用例等于没测");
    }

    for (job, d) in [("status", fresh_s), ("rebuild", built_s), ("status", built_s)] {
        let mut child = Command::new(bin())
            .args(["index", job, "--data-dir", d])
            .stdout(Stdio::piped())
            .spawn()
            .expect("hub binary");
        drop(child.stdout.take()); // 读端就此关闭 ⇒ 子进程那一侧是断管
        let code = child.wait().expect("wait").code();
        assert_eq!(code, Some(0), "提前关闭的管道把成功作业报成了失败：{job}");
    }
}

/// 证伪 8 的载体：加 `Index` 臂必须同步补那句自拼的用法串，漏了就是给操作员留一份说谎的用法行。
/// 计划点名"它一开始是红的"——红在用法行还没更新的那一刻，才是这条守卫的 killable 证明。
#[test]
fn 未知子命令的用法行必须列出_index_rebuild() {
    let out = run_hub(&["index", "bogus"]);
    assert_eq!(out.status.code(), Some(3), "未知子命令是用法错误，不是运维成功");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("invalid invocation"), "类别要报得出来：{err}");
    assert!(err.contains("index rebuild"), "用法行没跟着新臂更新：{err}");
    // 自拼串的另一半判据（与 `import_cli.rs` 的同名规则一致）：不回显操作员敲的那个 token。
    assert!(!err.contains("bogus"), "用法错误回显了操作员输入的 token：{err}");
    assert!(out.stdout.is_empty(), "用法错误不许打印任何东西到 stdout");
}
