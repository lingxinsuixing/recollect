//! v2 契约文件 -> v3 契约文件。只做结构迁移，不跑抽取、不读源格式。
//!
//! 用法：migrate_v2_to_v3 <input.v2.json> <output.v3.json> [--observed-at-ms N] [--adapter-version N]

use rc_contract::ingest_v2::IngestFile;
use rc_contract::ingest_v3::validate_ingest_v3;
use rc_contract::ingest_v3_migrate::{migrate_v2_to_v3, MigrateOptions, MigrationIssue};
use sha2::{Digest, Sha256};
use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let mut positional: Vec<String> = Vec::new();
    let mut observed_at_ms = None;
    let mut adapter_version = 1u32;
    let mut iter = env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--observed-at-ms" => {
                let Some(value) = iter.next() else {
                    usage();
                };
                observed_at_ms = Some(match value.parse::<i64>() {
                    Ok(value) => value,
                    Err(_) => usage(),
                });
            }
            "--adapter-version" => {
                let Some(value) = iter.next() else {
                    usage();
                };
                adapter_version = match value.parse::<u32>() {
                    Ok(value) if value >= 1 => value,
                    _ => usage(),
                };
            }
            _ if arg.starts_with("--") => usage(),
            _ => positional.push(arg),
        }
    }
    if positional.len() != 2 {
        usage();
    }
    let input = &positional[0];
    let output = &positional[1];
    let bytes = match fs::read(input) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("cannot read {input}: {error}");
            std::process::exit(2);
        }
    };
    let digest = Sha256::digest(&bytes);
    let source_sha256: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let v2: IngestFile = match serde_json::from_slice(&bytes) {
        Ok(file) => file,
        Err(error) => {
            print_error("parse", format!("v2 parse failed: {error}"));
        }
    };
    let options = MigrateOptions {
        source_path: input.clone(),
        source_sha256,
        adapter_version,
        ingest_version: 3,
        observed_at_ms,
    };
    let file = match migrate_v2_to_v3(&v2, &options) {
        Ok(file) => file,
        Err(issues) => {
            print_issues(&issues);
        }
    };
    let report = validate_ingest_v3(&file);
    if let Some(parent) = Path::new(output).parent() {
        if !parent.as_os_str().is_empty() {
            if let Err(error) = fs::create_dir_all(parent) {
                eprintln!("cannot create {}: {error}", parent.display());
                std::process::exit(2);
            }
        }
    }
    let text = serde_json::to_string_pretty(&file).expect("v3 file serializes");
    if let Err(error) = fs::write(output, text + "\n") {
        eprintln!("cannot write {output}: {error}");
        std::process::exit(2);
    }
    let out = serde_json::json!({
        "ok": report.ok,
        "input": input,
        "output": output,
        "source_sha256": options.source_sha256,
        "sessions": file.sessions.len(),
        "events": file.sessions.iter().map(|session| session.events.len()).sum::<usize>(),
        "errors": report.errors,
        "warnings": report.warnings.len(),
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(if report.ok { 0 } else { 1 });
}

fn usage() -> ! {
    eprintln!(
        "usage: migrate_v2_to_v3 <input.v2.json> <output.v3.json> [--observed-at-ms N] [--adapter-version N]"
    );
    std::process::exit(2);
}

fn print_error(code: &str, message: String) -> ! {
    let out = serde_json::json!({"ok": false, "error": code, "message": message});
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(1);
}

fn print_issues(issues: &[MigrationIssue]) -> ! {
    let out = serde_json::json!({"ok": false, "error": "migration", "issues": issues});
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    std::process::exit(1);
}
