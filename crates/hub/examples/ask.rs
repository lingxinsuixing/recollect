//! 一次性探针：读 `llm.toml` + 密钥箱，把给定的 system/user 发给第一个可用座位。
//!
//! 用法（不落库、不记 key、不出现在 shell 历史里）：
//!   cargo run --release -p hub --example ask -- \
//!     --llm-config llm.toml --data-dir . --system sys.txt --user user.txt --out reply.txt [--budget 4000]
//!
//! 座位按配置顺序尝试；失败就换下一座。回复原样写到 `--out`。

use rc_llm::{LlmBackend, Prompt};
use std::path::PathBuf;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut config, mut data_dir, mut system, mut user, mut out) = (None, None, None, None, None);
    let mut budget_override: Option<u32> = None;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--llm-config" => config = args.next().map(PathBuf::from),
            "--data-dir" => data_dir = args.next().map(PathBuf::from),
            "--system" => system = args.next(),
            "--user" => user = args.next(),
            "--out" => out = args.next(),
            "--budget" => budget_override = args.next().and_then(|s| s.parse().ok()),
            _ => {}
        }
    }
    let config = config.expect("--llm-config");
    let data_dir = data_dir.expect("--data-dir");
    let system = std::fs::read_to_string(system.expect("--system")).expect("system file");
    let user = std::fs::read_to_string(user.expect("--user")).expect("user file");
    let out = out.expect("--out");

    let cfg = rc_llm_cfg::load(&config)
        .expect("llm.toml parse")
        .expect("llm.toml missing");
    let deadline_ms = 600_000u64;

    for seat in &cfg.seats {
        let Some((box_name, slot)) = seat.secret_ref.split_once(':') else {
            eprintln!("bad secret_ref");
            continue;
        };
        let key = match rc_secret::read_secret(&data_dir, box_name, slot) {
            Ok(v) => v.to_string(),
            Err(e) => {
                eprintln!("secret: {e:?}");
                continue;
            }
        };
        let egress: Box<dyn rc_llm_http::Egress> = Box::new(rc_llm_http::transport::agent(deadline_ms));
        let budget = budget_override.or(seat.budget).unwrap_or(rc_llm_http::MAX_BUDGET);
        let live = match (&seat.provider, &seat.endpoint, seat.model.as_deref()) {
            (Some(p), None, Some(m)) => {
                rc_llm_http::Live::new(p, m, key, budget, seat.temperature, egress)
            }
            (None, Some(ep), Some(m)) => rc_llm_http::Live::new_custom(
                &ep.base_url, &ep.protocol, m, key, budget, seat.temperature, egress),
            _ => {
                eprintln!("seat shape unsupported");
                continue;
            }
        };
        let backend = match live {
            Ok(b) => b,
            Err(e) => {
                eprintln!("live: {e}");
                continue;
            }
        };
        let prompt = Prompt { system: system.clone(), user: user.clone(), derive: false };
        match backend.complete(&prompt) {
            Ok(reply) => {
                std::fs::write(&out, reply).expect("write out");
                println!("ok {}", seat.model.as_deref().unwrap_or("?"));
                return;
            }
            Err(e) => eprintln!("llm failed: {e:?}"),
        }
    }
    eprintln!("all seats failed");
    std::process::exit(2);
}
