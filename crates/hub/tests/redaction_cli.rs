//! 入库时值级脱敏在 CLI 面上的四条读数：计数、正文、身份、幂等，外加映射文件的"只在显式给了
//! 路径时才落盘"。守卫 12 的金丝雀在这里也走一遍：明文不许出现在 stdout/stderr 里。

use std::path::Path;
use std::process::Command;

use rc_contract::{compute_body_fp, compute_turn_uid};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

/// 一段正文里同时放三类样本：真凭据（必须换）、commit sha 与路径（必须留）、≥32 位数字（必须留）。
const CREDENTIAL: &str = "Abc12345xyz";
const COMMIT: &str = "439ef5c";
const PATH: &str = "/a/b/c.py";
const LONG_NUMBER: &str = "12345678901234567890123456789012";

fn body() -> String {
    format!(
        "服务器口令：password = {CREDENTIAL}\n修复在 commit {COMMIT}，文件 {PATH}\n构建号 {LONG_NUMBER}"
    )
}

fn fixture(dir: &Path) -> std::path::PathBuf {
    let body = body();
    let doc = serde_json::json!({
        "format_version": 1,
        "sessions": [{
            "session_key": "sess-redact",
            "device_id": "dev-redact",
            "source_agent": "qoder",
            "turns": [{
                "role": "human",
                "body": body,
                "created_at_ms": 1_800_000_000_000i64
            }]
        }]
    });
    let p = dir.join("redact.json");
    std::fs::write(&p, serde_json::to_vec(&doc).unwrap()).unwrap();
    p
}

fn run(file: &Path, data_dir: &Path, extra: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(bin())
        .args(["import", file.to_str().unwrap(), "--data-dir", data_dir.to_str().unwrap(), "--json"])
        .args(extra)
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap(),
    )
}

fn stored_turn(dir: &Path) -> (String, String, String) {
    let root = rc_store::Root::open(dir).unwrap();
    root.transcript()
        .conn()
        .query_row("SELECT turn_uid, body, body_fp FROM turns", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .unwrap()
}

/// 验收 2 + 4 的库里读数：正文真被换了、计数报得出来、身份仍由原文算。
#[test]
fn import_redacts_body_reports_count_and_keeps_identity() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(dir.path());
    let dd = dir.path().join("data");
    let (code, text) = run(&f, &dd, &[]);
    assert_eq!(code, Some(0), "{text}");

    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["turns_accepted"], 1, "{text}");
    assert_eq!(v["redacted_spans"], 1, "计数要进导入输出：{text}");
    for leak in [CREDENTIAL, "password ="] {
        assert!(!text.contains(leak), "输出泄露明文 {leak}：{text}");
    }
    // 阴性对照在 CLI 面上也成立：sha/路径/长数字没有被"顺手"报出来，也没被写走样。
    assert!(!text.contains(COMMIT) && !text.contains(PATH) && !text.contains(LONG_NUMBER));

    let (uid, body, fp) = stored_turn(&dd);
    assert!(body.contains("<redacted:credential>"), "库里正文没脱敏：{body}");
    assert!(!body.contains(CREDENTIAL), "库里残留明文：{body}");
    assert!(body.contains(COMMIT), "commit sha 被误伤：{body}");
    assert!(body.contains(PATH), "路径被误伤：{body}");
    assert!(body.contains(LONG_NUMBER), "长数字被误伤：{body}");

    let original = body_for_hash();
    assert_eq!(
        uid,
        compute_turn_uid("dev-redact", "sess-redact", "human", &original, 1_800_000_000_000),
        "turn_uid 必须仍由原文算（否则重复导入不再幂等）"
    );
    assert_eq!(fp, compute_body_fp(&original), "body_fp 必须仍由原文算");
}

// `normalize_body` 对这份样本是恒等变换（无 CRLF、无首尾空白），直接用它算期望值。
fn body_for_hash() -> String {
    body()
}

/// 验收 4：同一份文件再导一次仍走 `skipped_duplicate`（身份没被脱敏改掉）。
#[test]
fn reimport_is_still_skipped_duplicate() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(dir.path());
    let dd = dir.path().join("data");
    let (code, _) = run(&f, &dd, &[]);
    assert_eq!(code, Some(0));
    let (code, text) = run(&f, &dd, &[]);
    assert_eq!(code, Some(0), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["turns_accepted"], 0, "{text}");
    assert_eq!(v["skipped_duplicate"], 1, "脱敏改动了身份 ⇒ 这里会变成 1 条新轮：{text}");
    assert_eq!(v["redacted_spans"], 1, "重导同样计数（同一段正文又被扫了一遍）");
}

/// 映射只在 `--redaction-map` 显式给出时落盘；默认路径一个明文副本都不留。
#[test]
fn redaction_map_is_written_only_on_explicit_path() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(dir.path());
    let dd = dir.path().join("data");
    let absent = dir.path().join("never.jsonl");
    let (code, _) = run(&f, &dd, &[]);
    assert_eq!(code, Some(0));
    assert!(!absent.exists(), "没给路径也写了映射文件");

    let map = dir.path().join("map.jsonl");
    let (code, text) = run(&f, &dd, &["--redaction-map", map.to_str().unwrap()]);
    assert_eq!(code, Some(0), "{text}");
    assert!(!text.contains(CREDENTIAL), "报告里带出了明文：{text}");
    let raw = std::fs::read_to_string(&map).unwrap();
    let lines: Vec<&str> = raw.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1, "一段命中该有一行映射：{raw}");
    let row: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(row["original"], CREDENTIAL, "映射文件该留原文：{row}");
    assert_eq!(row["kind"], "keyword_value");
    assert_eq!(row["role"], "human");
    let (uid, _, _) = stored_turn(&dd);
    assert_eq!(row["turn_uid"], uid, "映射要能按 turn 对回去");
}

/// 映射写失败是运维失败（3），且错误只回类别名：路径与明文都不进输出。
#[test]
fn unwritable_redaction_map_exits_3_without_leaking() {
    let dir = tempfile::tempdir().unwrap();
    let f = fixture(dir.path());
    let dd = dir.path().join("data");
    // 拿一个目录当映射文件 ⇒ create 必失败。
    let bad = dir.path().join("a-directory");
    std::fs::create_dir(&bad).unwrap();
    let out = Command::new(bin())
        .args(["import", f.to_str().unwrap(), "--data-dir", dd.to_str().unwrap(), "--json"])
        .args(["--redaction-map", bad.to_str().unwrap()])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(text.contains("redaction map"), "类别要报得出来：{text}");
    for leak in [CREDENTIAL, "a-directory", dir.path().to_str().unwrap()] {
        assert!(!text.contains(leak), "输出泄露 {leak}：{text}");
    }
}
