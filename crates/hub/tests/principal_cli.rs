//! 身份层 I1：`hub principal claim|list` 的 CLI 面（工单 `2026-10-28-principal-i1` 判据 2）。
//!
//! 判定面是**真进程的 stdout + 库里的行**：命令有没有接线、退出码对不对、台账里两章的
//! `supersedes` 链能不能读出来，只有这两样看得见。
//!
//! 口径（与 `rc_ledger::transcript::claim_principal` 同一句话）：
//! - 只追加不更新；`--source correction` 未给 `--reason` ⇒ 退出 3（不落行）；
//! - `source` 是闭集（`rc_contract::PRINCIPAL_SOURCES`），闭集外是用法错误（退出 3）；
//! - 导入带 `owner_principal_id` ⇒ 自动落一条章，`claimed_by` = 该会话的 `source_agent`；
//! - 输出零路径、零正文（守卫 12 那一族：`claimed_by`/`principal` 是身份值，不是正文）。

use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    Command::new(bin()).args(args).output().expect("hub binary")
}

fn write_fixture(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body).unwrap();
    p
}

fn out_text(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn claims_json(data_dir: &Path, extra: &[&str]) -> Vec<Value> {
    let dd = data_dir.to_str().unwrap();
    let mut args = vec!["principal", "list", "--json", "--data-dir", dd];
    args.extend_from_slice(extra);
    let out = run(&args);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_str(&out_text(&out)).unwrap();
    v.as_array().cloned().unwrap_or_default()
}

#[test]
fn claim_then_list_shows_the_latest_claim_and_the_supersedes_chain() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();

    let first = run(&[
        "principal", "claim", "--session", "ses-1", "--device", "dev-1", "--principal", "acct-1",
        "--name", "小明", "--source", "agent_runtime", "--data-dir", dd,
    ]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let first_text = out_text(&first);
    assert!(first_text.contains("principal=acct-1"), "{first_text}");
    assert!(first_text.contains("source=agent_runtime"), "{first_text}");
    assert!(first_text.contains("supersedes=-"), "首条章没有可覆盖的行：{first_text}");
    assert!(!first_text.contains(dd), "输出泄露路径：{first_text}");

    let second = run(&[
        "principal", "claim", "--session", "ses-1", "--device", "dev-1", "--principal", "acct-2",
        "--source", "correction", "--reason", "上一章盖错了账号", "--data-dir", dd,
    ]);
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    let second_text = out_text(&second);
    assert!(second_text.contains("supersedes=1"), "改盖必须指回旧行：{second_text}");
    assert!(second_text.contains("reason=上一章盖错了账号"), "{second_text}");

    let arr = claims_json(dir.path(), &[]);
    assert_eq!(arr.len(), 2, "两章两行（追加不覆盖）");
    assert_eq!(arr[0]["principal_id"], "acct-2");
    assert_eq!(arr[0]["latest"], true);
    assert_eq!(arr[0]["supersedes"], arr[1]["id"]);
    assert_eq!(arr[0]["source"], "correction");
    assert_eq!(arr[0]["claimed_by"], "hub", "运维面盖的章与被导入自动盖的分得开");
    assert_eq!(arr[1]["principal_id"], "acct-1");
    assert_eq!(arr[1]["principal_name"], "小明");
    assert_eq!(arr[1]["latest"], false);

    // 人读的那一面：最新在前、`latest=1/0` 逐行可读。
    let text = out_text(&run(&[
        "principal", "list", "--session", "ses-1", "--data-dir", dd,
    ]));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with("latest=1 "), "{text}");
    assert!(lines[0].contains("principal=acct-2"), "{text}");
    assert!(lines[1].starts_with("latest=0 "), "{text}");

    // `--session` 过滤：别的会话的章不进这一屏。
    let other = run(&[
        "principal", "claim", "--session", "ses-2", "--device", "dev-1", "--principal", "acct-3",
        "--source", "human_claim", "--data-dir", dd,
    ]);
    assert!(other.status.success());
    assert_eq!(claims_json(dir.path(), &["--session", "ses-1"]).len(), 2);
    assert_eq!(claims_json(dir.path(), &["--session", "ses-2"]).len(), 1);
    assert_eq!(claims_json(dir.path(), &[]).len(), 3);
}

#[test]
fn correction_without_a_reason_exits_3_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();

    let refused = run(&[
        "principal", "claim", "--session", "ses-1", "--device", "dev-1", "--principal", "acct-2",
        "--source", "correction", "--data-dir", dd,
    ]);
    assert_eq!(refused.status.code(), Some(3), "correction 缺 reason 是运维失败（退出 3）");
    let se = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(se.contains("correction requires --reason"), "{se}");
    assert!(refused.stdout.is_empty(), "被拒的盖章不印一行成功读数");
    assert!(claims_json(dir.path(), &[]).is_empty(), "被拒的章一行都不许落");

    // 空白 reason 与没给是同一件事。
    let blank = run(&[
        "principal", "claim", "--session", "ses-1", "--device", "dev-1", "--principal", "acct-2",
        "--source", "correction", "--reason", "   ", "--data-dir", dd,
    ]);
    assert_eq!(blank.status.code(), Some(3));
    assert!(claims_json(dir.path(), &[]).is_empty());
}

#[test]
fn an_unknown_source_is_a_usage_error_naming_principal() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    let out = run(&[
        "principal", "claim", "--session", "ses-1", "--device", "dev-1", "--principal", "acct-1",
        "--source", "guess", "--data-dir", dd,
    ]);
    assert_eq!(out.status.code(), Some(3), "闭集外的 source 是用法错误");
    let se = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(se.contains("principal claim"), "用法串里必须有这条命令：{se}");
    assert!(!se.contains(dd), "输出泄露路径：{se}");
}

#[test]
fn an_import_carrying_a_principal_stamps_a_claim_for_that_session() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    let fixture = write_fixture(
        dir.path(),
        "principal.json",
        r#"{"format_version":1,"sessions":[{"session_key":"ses-imp","device_id":"dev-imp",
            "source_agent":"qoder-cli","owner_principal_id":"acct-42","principal_name":"小明",
            "principal_source":"human_claim",
            "turns":[{"role":"human","body":"CANARY-PRINCIPAL-BODY","created_at_ms":1730000000000}]}]}"#,
    );

    let fut = fixture.to_str().unwrap();
    let first = run(&["import", fut, "--data-dir", dd, "--json"]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    let arr = claims_json(dir.path(), &["--session", "ses-imp"]);
    assert_eq!(arr.len(), 1, "导入带身份 ⇒ 自动落一条章");
    assert_eq!(arr[0]["principal_id"], "acct-42");
    assert_eq!(arr[0]["principal_name"], "小明");
    assert_eq!(arr[0]["source"], "human_claim");
    assert_eq!(arr[0]["claimed_by"], "qoder-cli", "claimed_by = 该会话的 source_agent");
    assert_eq!(arr[0]["latest"], true);

    // 重传同一份文件：不长第二行。
    let again = run(&["import", fut, "--data-dir", dd, "--json"]);
    assert!(again.status.success());
    assert_eq!(claims_json(dir.path(), &["--session", "ses-imp"]).len(), 1);

    // 守卫 12：正文与路径都不许出现在 list 的输出里。
    let listed = out_text(&run(&["principal", "list", "--json", "--data-dir", dd]));
    assert!(!listed.contains("CANARY-PRINCIPAL-BODY"), "{listed}");
    assert!(!listed.contains(dd), "{listed}");
}
