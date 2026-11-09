//! 入库闸 v3：读 v3 契约文件，做结构/身份/P0/秘密扫描，输出 JSON 报告。
//! 退出码：0 通过，1 校验失败，2 用法/读取错误。
//!
//! `--require-p0` 面向生产入口：要求 `policy` 与 `key_manifest` 存在。
//! `replication` 属于备份/复制包，不在这里强制。

use rc_contract::ingest_v3::{validate_ingest_v3_with, V3File};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;

fn main() {
    let mut path = None;
    let mut require_p0 = false;
    for arg in env::args().skip(1) {
        match arg.as_str() {
            "--require-p0" => require_p0 = true,
            _ if arg.starts_with("--") => {
                eprintln!("usage: ingest_validate_v3 <file.json> [--require-p0]");
                std::process::exit(2);
            }
            _ => {
                if path.replace(arg).is_some() {
                    eprintln!("usage: ingest_validate_v3 <file.json> [--require-p0]");
                    std::process::exit(2);
                }
            }
        }
    }
    let Some(path) = path else {
        eprintln!("usage: ingest_validate_v3 <file.json> [--require-p0]");
        std::process::exit(2);
    };
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {path}: {error}");
            std::process::exit(2);
        }
    };
    let digest = Sha256::digest(&bytes);
    let sha: String = digest.iter().map(|b| format!("{b:02x}")).collect();

    let file: V3File = match serde_json::from_slice(&bytes) {
        Ok(file) => file,
        Err(error) => {
            let out = serde_json::json!({
                "ok": false,
                "input": path,
                "sha256": sha,
                "require_p0": require_p0,
                "error": "parse",
                "message": error.to_string(),
            });
            println!("{}", serde_json::to_string_pretty(&out).unwrap());
            std::process::exit(1);
        }
    };

    let report = validate_ingest_v3_with(&file, require_p0);
    let out = serde_json::json!({
        "ok": report.ok,
        "input": path,
        "sha256": sha,
        "format_version": file.format_version,
        "ingest_version": file.ingest_version,
        "require_p0": require_p0,
        "stats": report.stats,
        "errors": report.errors,
        "warnings": report.warnings,
        "quarantined": report.quarantined,
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(if report.ok { 0 } else { 1 });
}
