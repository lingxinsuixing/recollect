//! `recollect-eval` 的命令面。子命令与派发档 §二 逐字相同，另加 D1 之后的 `pass`：
//!
//! ```text
//! recollect-eval seed --out DIR --rows 200|1000|10000
//! recollect-eval recall --data-dir DIR --truth FILE --k 5 [--json]
//! recollect-eval inject --data-dir DIR --truth FILE [--json]
//! recollect-eval extract --data-dir DIR --truth FILE [--json]
//! recollect-eval threshold --data-dir DIR --truth FILE --k 5 [--json]
//! recollect-eval pass --gold FILE [--pass fact|event|time|derive] [--json]
//! ```
//!
//! 读库命令一律经 `rc_store::open_replica`（只读句柄）：尺子量库，不改库。
//! `pass` 是**纯离线**的一支：只读 GOLD-SPEC JSONL，不连库、不调模型。
//! 退出码：0 达标、1 读/写故障、2 用法、**3 = 量出来了但不达标**（inject 任一不命中 / extract 覆盖率缺一条）。
//! `threshold` 只给读数（曲线与选定阈值），不设达标线 ⇒ 一律 0；`pass` 同理，
//! 它只出报表，不设达标线，格式错才是 2。

use eval::gold::{self, Pass};
use eval::{
    measure_recall, measure_thresholds, read_truth, run_extract, run_inject, seed_library,
    EvalError, ExtractTruth, InjectTruth, RecallOutcome, RecallTruth, ThresholdOutcome,
};
use rc_store::{open_replica, MemoryHandle, Root};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
recollect-eval seed --out DIR --rows 200|1000|10000
recollect-eval recall --data-dir DIR --truth FILE --k 5 [--json]
recollect-eval inject --data-dir DIR --truth FILE [--json]
recollect-eval extract --data-dir DIR --truth FILE [--json]
recollect-eval threshold --data-dir DIR --truth FILE --k 5 [--json]
recollect-eval pass --gold FILE [--pass fact|event|time|derive] [--json]";

enum Command {
    Seed { out: PathBuf, rows: usize },
    Recall { data_dir: PathBuf, truth: PathBuf, k: usize, json: bool },
    Inject { data_dir: PathBuf, truth: PathBuf, json: bool },
    Extract { data_dir: PathBuf, truth: PathBuf, json: bool },
    Threshold { data_dir: PathBuf, truth: PathBuf, k: usize, json: bool },
    Pass { gold: PathBuf, pass: Option<Pass>, json: bool },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("{e}");
            if matches!(e, EvalError::Usage(_)) {
                eprintln!("{USAGE}");
            }
            ExitCode::from(e.exit_code())
        }
    }
}

fn run() -> Result<u8, EvalError> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse(&args)? {
        Command::Seed { out, rows } => {
            let root = Root::open(&out).map_err(|_| EvalError::SeedFailed)?;
            let seeded = seed_library(root.memory(), rows, now_ms())?;
            // seed 的输出固定是这一个对象（派发档 §二）。
            println!("{}", serde_json::json!({ "seeded": seeded }));
            Ok(0)
        }
        Command::Recall { data_dir, truth, k, json } => {
            let m = open_memory(&data_dir)?;
            let truth: RecallTruth = read_truth(&truth)?;
            let outcome = measure_recall(&m, &truth, k, now_ms())?;
            if json {
                print_json(&outcome)?;
            } else {
                print_recall(&outcome);
            }
            Ok(0)
        }
        Command::Inject { data_dir, truth, json } => {
            let m = open_memory(&data_dir)?;
            let truth: InjectTruth = read_truth(&truth)?;
            let outcome = run_inject(&m, &truth, now_ms())?;
            if json {
                print_json(&outcome)?;
            } else {
                print_inject(&outcome);
            }
            Ok(if outcome.ok { 0 } else { 3 })
        }
        Command::Extract { data_dir, truth, json } => {
            let m = open_memory(&data_dir)?;
            let truth: ExtractTruth = read_truth(&truth)?;
            let outcome = run_extract(&m, &truth)?;
            if json {
                print_json(&outcome)?;
            } else {
                print_extract(&outcome);
            }
            Ok(if outcome.ok { 0 } else { 3 })
        }
        Command::Threshold { data_dir, truth, k, json } => {
            let m = open_memory(&data_dir)?;
            let truth: RecallTruth = read_truth(&truth)?;
            let outcome = measure_thresholds(&m, &truth, k, now_ms())?;
            if json {
                print_json(&outcome)?;
            } else {
                print_threshold(&outcome);
            }
            Ok(0)
        }
        Command::Pass { gold, pass, json } => {
            // 纯离线：读文件、算指标、出报表。不 open_memory、不调模型。
            let rows = gold::read_gold(&gold)?;
            let report = gold::evaluate(&rows, pass);
            if json {
                print_json(&report)?;
            } else {
                print_gold(&report);
            }
            Ok(0)
        }
    }
}

fn open_memory(data_dir: &Path) -> Result<MemoryHandle, EvalError> {
    // 只读副本句柄：文件不在 = "还没播种"，schema 不对 = "不是这套产物写的"，
    // 两种都折成封闭码，不把 StoreError 的路径原文带出去。
    open_replica(&data_dir.join("memory.db")).map_err(|_| EvalError::DataUnavailable)
}

fn parse(args: &[String]) -> Result<Command, EvalError> {
    let (command, rest) = args.split_first()
        .ok_or(EvalError::Usage("a subcommand is required"))?;
    let mut flags: BTreeMap<String, String> = BTreeMap::new();
    let mut json = false;
    let mut i = 0_usize;
    while i < rest.len() {
        match rest[i].as_str() {
            "--json" => {
                json = true;
                i += 1;
            }
            flag @ ("--out" | "--rows" | "--data-dir" | "--truth" | "--k" | "--gold" | "--pass") => {
                let value = rest.get(i + 1)
                    .ok_or(EvalError::Usage("a flag is missing its value"))?;
                if flags.insert(flag.to_string(), value.clone()).is_some() {
                    return Err(EvalError::Usage("a flag is given twice"));
                }
                i += 2;
            }
            _ => return Err(EvalError::Usage("unrecognized flag")),
        }
    }
    let command = match command.as_str() {
        "seed" => {
            let out = PathBuf::from(take(&mut flags, "--out", "seed needs --out")?);
            let rows = take(&mut flags, "--rows", "seed needs --rows")?
                .parse::<usize>()
                .map_err(|_| EvalError::Usage("--rows must be a number"))?;
            Command::Seed { out, rows }
        }
        "recall" => {
            let data_dir = PathBuf::from(take(&mut flags, "--data-dir", "recall needs --data-dir")?);
            let truth = PathBuf::from(take(&mut flags, "--truth", "recall needs --truth")?);
            let k = match flags.remove("--k") {
                Some(raw) => raw.parse::<usize>()
                    .map_err(|_| EvalError::Usage("--k must be a positive integer"))?,
                None => 5,
            };
            if k == 0 {
                return Err(EvalError::Usage("--k must be at least 1"));
            }
            Command::Recall { data_dir, truth, k, json }
        }
        "inject" => {
            let data_dir = PathBuf::from(take(&mut flags, "--data-dir", "inject needs --data-dir")?);
            let truth = PathBuf::from(take(&mut flags, "--truth", "inject needs --truth")?);
            Command::Inject { data_dir, truth, json }
        }
        "extract" => {
            let data_dir = PathBuf::from(take(&mut flags, "--data-dir", "extract needs --data-dir")?);
            let truth = PathBuf::from(take(&mut flags, "--truth", "extract needs --truth")?);
            Command::Extract { data_dir, truth, json }
        }
        "threshold" => {
            let data_dir = PathBuf::from(take(&mut flags, "--data-dir", "threshold needs --data-dir")?);
            let truth = PathBuf::from(take(&mut flags, "--truth", "threshold needs --truth")?);
            let k = match flags.remove("--k") {
                Some(raw) => raw.parse::<usize>()
                    .map_err(|_| EvalError::Usage("--k must be a positive integer"))?,
                None => 5,
            };
            if k == 0 {
                return Err(EvalError::Usage("--k must be at least 1"));
            }
            Command::Threshold { data_dir, truth, k, json }
        }
        "pass" => {
            let gold = PathBuf::from(take(&mut flags, "--gold", "pass needs --gold")?);
            // 不给 `--pass` 时按文件内的 pass 分组；给了就只出那一组（闭集外的值直接是用法错）。
            let pass = match flags.remove("--pass") {
                Some(raw) => Some(
                    Pass::parse(&raw)
                        .ok_or(EvalError::Usage("--pass must be fact, event, time or derive"))?,
                ),
                None => None,
            };
            Command::Pass { gold, pass, json }
        }
        _ => return Err(EvalError::Usage("unknown subcommand")),
    };
    if !flags.is_empty() {
        return Err(EvalError::Usage("a flag is not valid for this subcommand"));
    }
    Ok(command)
}

fn take(flags: &mut BTreeMap<String, String>, name: &str, why: &'static str)
    -> Result<String, EvalError> {
    flags.remove(name).ok_or(EvalError::Usage(why))
}

fn print_json<T: Serialize>(value: &T) -> Result<(), EvalError> {
    let line = serde_json::to_string(value).map_err(|_| EvalError::WriteFailed)?;
    println!("{line}");
    Ok(())
}

fn print_recall(o: &RecallOutcome) {
    println!("queries: {}  k: {}", o.queries, o.k);
    println!("recall@k: {:.4}", o.recall_at_k);
    println!("precision@k: {:.4}", o.precision_at_k);
    println!("mrr: {:.4}", o.mrr);
    for q in &o.per_query {
        println!(
            "  {} => recall {:.4}, precision {:.4}, rr {:.4}",
            q.query, q.recall_at_k, q.precision_at_k, q.reciprocal_rank
        );
    }
}

fn print_inject(o: &eval::InjectOutcome) {
    println!("cases: {}  ok: {}", o.cases, o.ok);
    println!("hit: {}/{} ({:.4})", o.hit.passed, o.hit.total, o.hit.rate);
    println!("silent: {}/{} ({:.4})", o.silent.passed, o.silent.total, o.silent.rate);
    println!(
        "anti_noise: {}/{} ({:.4})",
        o.anti_noise.passed, o.anti_noise.total, o.anti_noise.rate
    );
    for case in &o.per_case {
        println!(
            "  {:?} {} => {} (candidates {})",
            case.kind,
            case.query,
            if case.ok { "ok" } else { "miss" },
            case.candidates
        );
    }
}

fn print_extract(o: &eval::ExtractOutcome) {
    println!("turns: {}", o.turns);
    println!("expected: {}/{}", o.expected_found, o.expected_total);
    println!(
        "provenance_coverage: {}/{} ({:.4})",
        o.provenance_with_origin, o.provenance_rows, o.provenance_coverage
    );
    println!("must_keep: {}  false_clear: {}", o.must_keep, o.false_clear);
    println!("ok: {}", o.ok);
}

fn print_threshold(o: &ThresholdOutcome) {
    println!(
        "queries: {}  measurable: {}  no_relevant_queries: {}  k: {}",
        o.queries,
        o.queries - o.no_relevant_queries,
        o.no_relevant_queries,
        o.k
    );
    println!("  t     precision  recall   f1       kept  silenced");
    for p in &o.thresholds {
        println!(
            "  {:<5} {:.4}     {:.4}   {:.4}   {:.2}  {}",
            p.t, p.precision, p.recall, p.f1, p.kept, p.silenced
        );
    }
    match o.looser {
        Some(t) => println!("selected: {}  looser: {}", o.selected, t),
        None => println!("selected: {}  looser: none", o.selected),
    }
}

/// 分母为 0 的指标打 `null`（派发档 §二）：**不**打 0.0000，也不打 1.0000。
fn print_rate(value: Option<f64>) -> String {
    match value {
        Some(x) => format!("{x:.4}"),
        None => "null".to_string(),
    }
}

fn print_gold(report: &gold::GoldReport) {
    if report.passes.is_empty() {
        println!("passes: 0  (no gold rows in file)");
        return;
    }
    for p in &report.passes {
        println!("pass: {}  rows: {}", p.pass.as_str(), p.rows);
        println!(
            "drafts: total {}  judged {}  positive {}  precision {}",
            p.drafts.total,
            p.drafts.judged,
            p.drafts.positive,
            print_rate(p.drafts.precision)
        );
        for (verdict, count) in &p.drafts.verdicts {
            println!("  {verdict}: {count}");
        }
        let t = &p.turn_expectations;
        println!(
            "turn_expectations: total {}  should_yield {}  should_not_yield {}",
            t.total, t.should_yield, t.should_not_yield
        );
        println!(
            "  recall: {}/{} = {}  false_yield: {}/{} = {}",
            t.yielded,
            t.should_yield,
            print_rate(t.recall),
            t.false_yielded,
            t.should_not_yield,
            print_rate(t.false_yield)
        );
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
