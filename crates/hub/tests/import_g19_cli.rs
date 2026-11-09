//! G19 刀 5/6 的 CLI 判据：import 多文件（逐文件回执 + 最坏退出码）与回执的
//! `turn_uids`（json 全量 / 文本截断明示）。单文件的行为与回执由
//! `import_cli.rs` 的既有判据钉住（回归红线），本文件只证多文件与 turn_uid 两刀。
//!
//! 夹具全部自造（合成会话/正文），不抄真实语料。

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn write_fixture(dir: &Path, name: &str, body_json: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body_json).unwrap();
    p
}

fn ok_fixture(session: &str, device: &str, body: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"{device}","source_agent":"qoder","turns":[{{"role":"human","body":"{body}","created_at_ms":1730000000000}}]}}]}}"#
    )
}

#[test]
fn multi_file_import_gives_per_file_reports_and_the_worst_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    let good1 = write_fixture(dir.path(), "a.json", &ok_fixture("ses-m1", "dev-m1", "G19 多文件夹具甲"));
    let good2 = write_fixture(dir.path(), "b.json", &ok_fixture("ses-m2", "dev-m2", "G19 多文件夹具乙"));
    let missing = dir.path().join("no-such-file.json");

    // 不可读的文件排**最后**：每个好文件独立一份 Report（json 一行一个），坏文件只在
    // stderr 出一条类别行（零路径纪律），退出码取最坏 = 3。
    let out = Command::new(bin())
        .args(["import", good1.to_str().unwrap(), good2.to_str().unwrap(),
               missing.to_str().unwrap(), "--data-dir", dd, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("cannot read input file"), "{stderr}");
    assert!(!stderr.contains("no-such-file"), "运维面回显了路径：{stderr}");

    let stdout = String::from_utf8(out.stdout.clone()).unwrap();
    let reports: Vec<serde_json::Value> = stdout.lines()
        .map(|l| serde_json::from_str(l).expect("每个文件一份独立 JSON 报告"))
        .collect();
    assert_eq!(reports.len(), 2, "两个可读文件各一份报告：{stdout}");
    assert_eq!(reports[0]["turns_accepted"], 1, "{reports:?}");
    assert_eq!(reports[1]["turns_accepted"], 1, "{reports:?}");
    for r in &reports {
        assert_eq!(r["turn_uids"].as_array().map(Vec::len), Some(1),
                   "接受轮的 uid 列表在 json 面必有：{r}");
    }
    // 两个会话都真的进了库（"其余继续"不是嘴上说说）。
    let root = rc_store::Root::open(dir.path()).unwrap();
    let n: i64 = root.transcript().conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 2, "两个文件的轮都该落地：{n}");
}

#[test]
fn multi_file_import_takes_the_worst_exit_not_the_last_one() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    // 整批拒的文件排**第一**，好文件排最后：最后一行报告是成功的，
    // 但最坏退出码必须是 2——"取最坏"不许被"取最后一个"冒名。
    let bad = write_fixture(dir.path(), "bad.json",
        r#"{"format_version":1,"sessions":[{"session_key":"s","device_id":"dev-b","source_agent":"a","turns":[{"role":"human","body":"","created_at_ms":1}]}]}"#);
    let good = write_fixture(dir.path(), "good.json", &ok_fixture("ses-ok", "dev-g", "G19 最坏码夹具"));

    let out = Command::new(bin())
        .args(["import", bad.to_str().unwrap(), good.to_str().unwrap(),
               "--data-dir", dd, "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    let reports: Vec<serde_json::Value> = String::from_utf8(out.stdout).unwrap().lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert_eq!(reports[0]["batch_reject"], "body_empty", "{reports:?}");
    assert_eq!(reports[0]["turn_uids"].as_array().map(Vec::len), Some(0),
               "整批拒收没有接受轮：{reports:?}");
    assert_eq!(reports[1]["turns_accepted"], 1, "{reports:?}");
}

#[test]
fn single_file_import_report_is_one_json_object_with_turn_uids() {
    // 单文件回归红线的可判形状：stdout 仍解析为**一个** JSON 对象（多文件才是
    // 一行一个），且第一行文本与既有格式逐字相同（新行只许是 turn_uid 那一行）。
    // 两次运行各用**独立数据目录**：同一座库里重导同一文件是幂等命中
    // （skipped_duplicate），那会把"首行逐字相同"的断言变成另一件事。
    let dir = tempfile::tempdir().unwrap();
    let f = write_fixture(dir.path(), "ok.json",
                          &ok_fixture("ses-single", "dev-single", "G19 单文件夹具"));

    let jdir = tempfile::tempdir().unwrap();
    let jout = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", jdir.path().to_str().unwrap(),
               "--json"])
        .output()
        .unwrap();
    assert!(jout.status.success());
    let text = String::from_utf8(jout.stdout).unwrap();
    let v: serde_json::Value = serde_json::from_str(&text)
        .expect("单文件 json 回执仍是单个对象");
    assert_eq!(v["turns_accepted"], 1);
    let uids = v["turn_uids"].as_array().unwrap();
    assert_eq!(uids.len(), 1, "{v}");
    let uid = uids[0].as_str().unwrap();
    assert_eq!(uid.len(), 64, "turn_uid 是 64 hex：{uid}");

    let tdir = tempfile::tempdir().unwrap();
    let tout = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", tdir.path().to_str().unwrap()])
        .output()
        .unwrap();
    assert!(tout.status.success());
    let stdout = String::from_utf8(tout.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0],
        "turns_accepted=1 turns_rejected=0 skipped_duplicate=0 batch_reject=none redacted_spans=0",
        "首行与既有格式逐字相同：{lines:?}");
    assert_eq!(lines[1], format!("turn_uids=1 {uid}"), "第二行是 turn_uid 行：{lines:?}");
}

#[test]
fn receipt_turn_uids_truncates_the_text_face_with_an_explicit_count() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    // 25 轮的夹具：json 面 25 条全量，文本面只列前 20 条并明示 (+5 more)。
    let mut turns = String::new();
    for i in 0..25 {
        if i > 0 { turns.push(','); }
        turns.push_str(&format!(
            r#"{{"role":"human","body":"G19 截断夹具第{i}条","created_at_ms":{}"#,
            1730000000000i64 + i));
        turns.push('}');
    }
    let f = write_fixture(dir.path(), "many.json",
        &format!(r#"{{"format_version":1,"sessions":[{{"session_key":"s25","device_id":"dev-25","source_agent":"a","turns":[{turns}]}}]}}"#));

    let jout = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dd, "--json"])
        .output()
        .unwrap();
    assert!(jout.status.success());
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8(jout.stdout).unwrap()).unwrap();
    let uids: Vec<String> = v["turn_uids"].as_array().unwrap()
        .iter().map(|u| u.as_str().unwrap().to_string()).collect();
    assert_eq!(uids.len(), 25, "json 面全量：{v}");
    assert_eq!(uids.iter().collect::<std::collections::HashSet<_>>().len(), 25,
               "uid 互不相同：{uids:?}");

    let tout = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dd])
        .output()
        .unwrap();
    assert!(tout.status.success());
    let text = String::from_utf8(tout.stdout).unwrap();
    let uid_line = text.lines().nth(1).expect("第二行是 turn_uid 行");
    assert!(uid_line.starts_with("turn_uids=25 "), "计数在明处：{uid_line}");
    assert!(uid_line.ends_with("(+5 more)"), "截断必须明示：{uid_line}");
    // 前 20 条在场，第 21 条不在场。
    assert_eq!(uid_line.matches(&uids[0]).count(), 1, "{uid_line}");
    assert!(uid_line.contains(uids[19].as_str()), "第 20 条在场：{uid_line}");
    assert!(!uid_line.contains(uids[20].as_str()), "第 21 条不进文本面：{uid_line}");
}

#[test]
fn multi_file_with_redaction_map_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    let a = write_fixture(dir.path(), "a.json", &ok_fixture("ses-r1", "dev-r1", "G19 映射夹具甲"));
    let b = write_fixture(dir.path(), "b.json", &ok_fixture("ses-r2", "dev-r2", "G19 映射夹具乙"));
    let map = dir.path().join("map.jsonl");
    let out = Command::new(bin())
        .args(["import", a.to_str().unwrap(), b.to_str().unwrap(),
               "--redaction-map", map.to_str().unwrap(), "--data-dir", dd])
        .output()
        .unwrap();
    // 映射文件按导入覆盖写：多文件逐份覆盖 = 销毁前几个文件的"哪一段被换"的唯一副本，
    // 所以响亮拒绝（exit 3），不碰库、不产报告。
    assert_eq!(out.status.code(), Some(3), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("redaction-map"), "{stderr}");
    assert!(out.stdout.is_empty(), "拒绝不产报告：{}", String::from_utf8_lossy(&out.stdout));
    let root = rc_store::Root::open(dir.path()).unwrap();
    let n: i64 = root.transcript().conn()
        .query_row("SELECT count(*) FROM turns", [], |r| r.get(0)).unwrap();
    assert_eq!(n, 0, "拒绝发生在任何文件动库之前：{n}");
}
