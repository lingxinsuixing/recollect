//! 离线探针：把一份 exchange 的会话喂给 A1 两跑，打印合并后的锚与计数。
//!
//! 用法（key 从 stdin 进，不进参数、不进 shell 历史）：
//!   cargo run --example a1_probe -- fixtures/a1/dev.json deepseek deepseek-chat
//!   cargo run --example a1_probe -- --custom-base https://api.example.com/v1 \
//!       --custom-protocol openai-chat fixtures/a1/dev.json custom model-slug
//!   cargo run --example a1_probe -- --plan-dir target/a1/dev fixtures/a1/dev.json deepseek deepseek-chat
//!
//! 它**不落库**：这一步只回答"模型产出的锚长什么样、两跑稳不稳"。落库走
//! `rc_memory::write_a1`（`tests/a1_run.rs` 的离线闭环已经覆盖）。

use rc_contract::{normalize, parse};
use rc_distill::batch::{Batch, TurnRow};
use rc_distill::contract_v4::plan_a1;
use rc_distill::prompt_v4::AnchorCandidate;
use rc_distill::run_a1_pair;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_llm_http::transport;
use rc_llm_http::Live;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::Read;

/// 探针独有：把两次回复原样留一份，失败时（或 `A1_PROBE_RAW=1` 时）打到 stderr。
/// 这个包装只住 examples，生产链路的 `run_a1_pair` 看不到它。
struct Capture<'a> {
    inner: &'a dyn LlmBackend,
    raws: RefCell<Vec<String>>,
}

impl<'a> Capture<'a> {
    fn new(inner: &'a dyn LlmBackend) -> Self {
        Capture { inner, raws: RefCell::new(Vec::new()) }
    }
}

impl LlmBackend for Capture<'_> {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        let out = self.inner.complete(prompt)?;
        self.raws.borrow_mut().push(out.clone());
        Ok(out)
    }
}

struct Args {
    plan_out: Option<String>,
    plan_dir: Option<String>,
    custom_base: Option<String>,
    custom_protocol: String,
    exchange: String,
    provider: String,
    model: String,
    budget: u32,
    all_sessions: bool,
    timeout_ms: u64,
    reasoning_effort: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: a1_probe [--plan-out PATH] [--plan-dir DIR] [--all-sessions] \
         [--custom-base URL] [--custom-protocol SLUG] [--budget N] [--timeout-ms MS] \
         [--reasoning-effort none|low|medium|high|max] \
         <exchange.json> <provider> <model> [budget]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut plan_out = None;
    let mut plan_dir = None;
    let mut custom_base = None;
    let mut custom_protocol = "openai-chat".to_string();
    let mut all_sessions = false;
    let mut budget_flag = None;
    let mut reasoning_effort = None;
    // 探针的单发墙钟：生产周期预算（50 分钟）会把一次挂起拖成整轮失败。
    let mut timeout_ms = std::env::var("A1_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600_000u64);
    let mut positional = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--plan-out" => plan_out = args.next(),
            "--plan-dir" => plan_dir = args.next(),
            "--custom-base" => custom_base = args.next(),
            "--custom-protocol" => custom_protocol = args.next().unwrap_or_else(|| usage()),
            "--budget" => {
                budget_flag = Some(
                    args.next()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or_else(|| usage()),
                )
            }
            "--timeout-ms" => {
                timeout_ms = args
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--all-sessions" => all_sessions = true,
            "--reasoning-effort" => reasoning_effort = args.next(),
            "--help" | "-h" => usage(),
            _ => positional.push(arg),
        }
    }
    if positional.len() < 3 {
        usage();
    }
    let budget = budget_flag
        .or_else(|| positional.get(3).and_then(|s| s.parse().ok()))
        .unwrap_or(32_000);
    let all = all_sessions || plan_dir.is_some();
    Args {
        plan_out,
        plan_dir,
        custom_base,
        custom_protocol,
        exchange: positional[0].clone(),
        provider: positional[1].clone(),
        model: positional[2].clone(),
        budget,
        all_sessions: all,
        timeout_ms,
        reasoning_effort,
    }
}

struct SessionGroup {
    session_key: String,
    device_id: String,
    source_agent: String,
    project_hint: Option<String>,
    rows: Vec<TurnRow>,
}

fn main() {
    let args = parse_args();
    let bytes = std::fs::read(&args.exchange).unwrap_or_else(|e| {
        eprintln!("cannot read {}: {e}", args.exchange);
        std::process::exit(1);
    });
    let file = parse(&bytes).unwrap_or_else(|rejects| {
        eprintln!(
            "exchange rejected: {} rejection(s), first={:?}",
            rejects.len(),
            rejects.first()
        );
        std::process::exit(1);
    });
    let report = normalize(file, 0);
    let mut groups: Vec<SessionGroup> = Vec::new();
    let mut index: BTreeMap<(String, String), usize> = BTreeMap::new();
    for accepted in &report.accepted {
        let key = (
            accepted.session.session_key.clone(),
            accepted.session.device_id.clone(),
        );
        let pos = match index.get(&key) {
            Some(pos) => *pos,
            None => {
                let pos = groups.len();
                index.insert(key, pos);
                groups.push(SessionGroup {
                    session_key: accepted.session.session_key.clone(),
                    device_id: accepted.session.device_id.clone(),
                    source_agent: accepted.session.source_agent.clone(),
                    project_hint: accepted.session.project_hint.clone(),
                    rows: Vec::new(),
                });
                pos
            }
        };
        groups[pos].rows.push(TurnRow {
            turn_uid: accepted.turn.turn_uid.clone(),
            role: accepted.turn.role.clone(),
            body: accepted.turn.body.clone(),
            created_at_ms: accepted.turn.created_at_ms,
            ingested_at_ms: accepted.turn.created_at_ms,
            speaker: None,
            speaker_anchor_id: None,
        });
    }
    if groups.is_empty() {
        eprintln!("no accepted turns");
        std::process::exit(1);
    }
    if !args.all_sessions {
        groups.truncate(1);
    }

    let mut key = String::new();
    std::io::stdin().read_to_string(&mut key).unwrap_or_else(|e| {
        eprintln!("cannot read key from stdin: {e}");
        std::process::exit(1);
    });
    let key = key.trim().to_string();
    let mut live = match &args.custom_base {
        Some(base) => Live::new_custom(
            base,
            &args.custom_protocol,
            &args.model,
            key,
            args.budget,
            Some(0.1),
            Box::new(transport::agent(args.timeout_ms)),
        ),
        None => Live::new(
            &args.provider,
            &args.model,
            key,
            args.budget,
            Some(0.1),
            Box::new(transport::agent(args.timeout_ms)),
        ),
    }
    .unwrap_or_else(|e| {
        eprintln!("live backend: {e}");
        std::process::exit(1);
    });
    if let Some(effort) = args.reasoning_effort.clone() {
        live = live.with_reasoning_effort(Some(effort));
    }

    if let Some(dir) = &args.plan_dir {
        std::fs::create_dir_all(dir).unwrap_or_else(|e| {
            eprintln!("cannot create {dir}: {e}");
            std::process::exit(1);
        });
    }

    let mut merged = rc_distill::a1_eval::PlanView {
        anchors: Vec::new(),
        mentions: Vec::new(),
        issues: Vec::new(),
        unresolved: Vec::new(),
    };
    let mut sessions_json = Vec::new();
    let mut failed = false;

    for (pos, group) in groups.iter().enumerate() {
        let batch = Batch {
            session_key: group.session_key.clone(),
            device_id: group.device_id.clone(),
            source_agent: group.source_agent.clone(),
            project_hint: group.project_hint.clone(),
            rows: group.rows.clone(),
        };
        let capture = Capture::new(&live);
        let mut retries = 0usize;
        let mut pair = run_a1_pair(&capture, &batch, &[] as &[AnchorCandidate], None);
        // 结构/契约失败只重试一次：用来区分“偶发不符合契约”与“系统性不会写”。
        // 生产链路的 run_a1_pair 不重试；重试次数会写进 sessions[]，读数里可见。
        while pair.is_err() && retries < 1 {
            retries += 1;
            eprintln!("contract retry {retries}/1 for {}", group.session_key);
            pair = run_a1_pair(&capture, &batch, &[] as &[AnchorCandidate], None);
        }
        if pair.is_err() || std::env::var("A1_PROBE_RAW").is_ok() {
            for (i, raw) in capture.raws.borrow().iter().enumerate() {
                eprintln!("--- raw reply {} ({}) ---\n{raw}\n---", i + 1, group.session_key);
            }
        }
        let (first, second) = match pair {
            Ok(pair) => pair,
            Err(e) => {
                failed = true;
                eprintln!("a1 failed for {}: {e}", group.session_key);
                sessions_json.push(serde_json::json!({
                    "session_key": group.session_key,
                    "turns": batch.rows.len(),
                    "calls": 2 + retries * 2,
                    "retries": retries,
                    "ok": false,
                    "error": e.to_string(),
                }));
                continue;
            }
        };
        let plan = plan_a1(&first, &second, &batch);
        let view = rc_distill::a1_eval::PlanView::from_batch_with_session(&plan, &group.session_key);
        let file_stem = format!("{:02}-{}", pos, safe_stem(&group.session_key));
        if let Some(dir) = &args.plan_dir {
            let path = std::path::Path::new(dir).join(format!("{file_stem}.json"));
            write_json(&path, &view);
        }
        if args.plan_out.is_some() {
            merge_view(&mut merged, &view);
        }
        sessions_json.push(serde_json::json!({
            "session_key": group.session_key,
            "turns": batch.rows.len(),
            "calls": 2 + retries * 2,
            "retries": retries,
            "ok": true,
            "anchors": view.anchors.len(),
            "stable": view.anchors.iter().filter(|a| a.stable).count(),
            "mentions": view.mentions.len(),
            "issues": view.issues.len(),
            "unresolved": view.unresolved.len(),
            "plan_file": args.plan_dir.as_ref().map(|_| format!("{file_stem}.json")),
        }));
    }

    if let Some(path) = &args.plan_out {
        write_json(std::path::Path::new(path), &merged);
    }

    let aggregate = serde_json::json!({
        "provider": if args.custom_base.is_some() { "custom" } else { args.provider.as_str() },
        "model": args.model,
        "custom": args.custom_base.is_some(),
        "budget": args.budget,
        "sessions": sessions_json,
        "failed": failed,
    });
    println!("{}", aggregate);
    if failed {
        std::process::exit(1);
    }
}

fn write_json<T: serde::Serialize>(path: &std::path::Path, value: &T) {
    let body = serde_json::to_vec_pretty(value).unwrap_or_else(|e| {
        eprintln!("cannot encode {}: {e}", path.display());
        std::process::exit(1);
    });
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).unwrap_or_else(|e| {
                eprintln!("cannot create {}: {e}", parent.display());
                std::process::exit(1);
            });
        }
    }
    std::fs::write(path, body).unwrap_or_else(|e| {
        eprintln!("cannot write {}: {e}", path.display());
        std::process::exit(1);
    });
}

/// 合并多 session 计划：ref 加 session 前缀，并保留 session_key 供评测器按会话判拆分。
fn merge_view(dst: &mut rc_distill::a1_eval::PlanView, src: &rc_distill::a1_eval::PlanView) {
    for a in &src.anchors {
        let mut a = a.clone();
        a.reference = format!("{}/{}", a.session_key, a.reference);
        dst.anchors.push(a);
    }
    for m in &src.mentions {
        let mut m = m.clone();
        m.anchor_ref = format!("{}/{}", m.session_key, m.anchor_ref);
        dst.mentions.push(m);
    }
    dst.issues.extend(src.issues.iter().cloned());
    dst.unresolved.extend(src.unresolved.iter().cloned());
}

fn safe_stem(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}
