//! 只读评测：`a1_eval <plan.json> <gold.json> <exchange.json>`。
//!
//! plan 由 `a1_probe --plan-out` 产出；gold 与 exchange 都是仓库里的 fixtures。
//! 输出一个 JSON 报告（核心召回、别名召回、kind 准确、拆分、幻影、禁名、额外锚、stable 覆盖，
//! 以及逐轮 mention/unresolved/结构闸）。它不调模型、不写库，同一输入重放得到同一报告。

use rc_distill::a1_eval::{evaluate_with_turns, turns_from_exchange, GoldFile, PlanView};
use serde_json::Value;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        eprintln!("usage: a1_eval <plan.json> <gold.json> <exchange.json>");
        std::process::exit(2);
    }
    let plan: PlanView = read_json(&args[0]);
    let gold: GoldFile = read_json(&args[1]);
    let doc: Value = read_json(&args[2]);
    let turns = turns_from_exchange(&doc);
    let report = evaluate_with_turns(&plan, &gold, &turns);
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn read_json<T: serde::de::DeserializeOwned>(path: &str) -> T {
    let bytes = std::fs::read(path).unwrap_or_else(|e| {
        eprintln!("cannot read {path}: {e}");
        std::process::exit(1);
    });
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        eprintln!("cannot parse {path}: {e}");
        std::process::exit(1);
    })
}
