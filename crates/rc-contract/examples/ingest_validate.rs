//! 入库闸：读 v2 契约文件，做结构/语义/秘密扫描，输出 JSON 报告。
//! 退出码：0 通过，1 校验失败，2 用法/读取错误。

use rc_contract::ingest_v2::{validate_ingest, IngestFile};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;

fn main() {
    let path = match env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("usage: ingest_validate <file.json>");
            std::process::exit(2);
        }
    };
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("cannot read {path}: {e}");
            std::process::exit(2);
        }
    };
    let digest = Sha256::digest(&bytes);
    let sha: String = digest.iter().map(|b| format!("{b:02x}")).collect();

    let file: IngestFile = match serde_json::from_slice(&bytes) {
        Ok(f) => f,
        Err(e) => {
            let out = serde_json::json!({
                "ok": false,
                "input": path,
                "sha256": sha,
                "error": "parse",
                "message": e.to_string(),
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            std::process::exit(1);
        }
    };

    let report = validate_ingest(&file);
    let out = serde_json::json!({
        "ok": report.ok,
        "input": path,
        "sha256": sha,
        "format_version": file.format_version,
        "stats": report.stats,
        "errors": report.errors,
        "warnings": report.warnings,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(if report.ok { 0 } else { 1 });
}
