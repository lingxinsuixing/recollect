//! `recollect-eval pass` 的判据（派发档 2026-10-28-gold-eval §判据 1 的 a–e + 一条字段溯源）。
//!
//! 五条用例**全部走 CLI 黑盒**（`CARGO_BIN_EXE_recollect-eval`），不 import `eval::gold`：
//! 这样同一份测试能 cp 到 BASE `5e1783a` 上编译并跑红——BASE 上没有 `pass` 子命令，
//! 而库层 API 在 BASE 上根本不存在，编译失败证明不了"尺子有牙"，只证明"代码不在"。
//!
//! 数值全是**手算**的：precision 2/4=0.5、recall 2/3、false_yield 1/2。改成恒 1 或恒 0 都会红。

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_recollect-eval");

/// 一条合法 draft 行（GOLD-SPEC §2 的逐字形状）。
fn draft(pass: &str, verdict: &str, item_id: i64) -> Value {
    json!({
        "kind": "draft", "pass": pass, "family": "decision",
        "item_id": item_id, "draft_index": 0,
        "turn_uids": ["00000000-0000-0000-0000-000000000000"],
        "draft": {
            "category": "decision",
            "subject_canonical": "qoder",
            "text": "合成草稿正文",
            "quote": "合成引文"
        },
        "verdict": verdict,
        "evidence": "证据轮正文：\"合成引文\"",
        "annotator": "fact-gold", "annotated_at": "2026-10-28"
    })
}

/// 一条合法 turn_expectation 行（GOLD-SPEC §3 的逐字形状）。
fn turn(pass: &str, expect: &str, actual_yielded: bool) -> Value {
    json!({
        "kind": "turn_expectation", "pass": pass, "family": "decision",
        "turn_uid": "11111111-1111-1111-1111-111111111111",
        "session_key": "qoder:synthetic",
        "expect": expect, "actual_yielded": actual_yielded,
        "evidence": "该轮正文：\"合成引文\"",
        "annotator": "fact-gold", "annotated_at": "2026-10-28"
    })
}

fn write_gold(dir: &Path, name: &str, rows: &[Value]) -> PathBuf {
    let path = dir.join(name);
    let mut text = String::new();
    for row in rows {
        text.push_str(&row.to_string());
        text.push('\n');
    }
    fs::write(&path, text).unwrap();
    path
}

fn run_pass(path: &Path, extra: &[&str]) -> Output {
    let mut args: Vec<String> =
        vec!["pass".into(), "--gold".into(), path.to_str().unwrap().into()];
    args.extend(extra.iter().map(|s| s.to_string()));
    Command::new(BIN).args(&args).output().unwrap()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn parse_json(output: &Output) -> Value {
    let stdout = stdout_of(output);
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| {
        panic!("stdout 不是单个 JSON 对象（{e}）：\n{stdout}\nstderr:\n{}", stderr_of(output))
    })
}

/// 判据 1a：合法文件的分组统计与 precision/recall 数值正确（手算）。
#[test]
fn gold_metrics_match_a_hand_computed_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_gold(
        dir.path(),
        "gold.jsonl",
        &[
            draft("fact", "supported", 100),
            draft("fact", "supported", 101),
            draft("fact", "wrong_subject", 102),
            draft("fact", "hallucinated", 103),
            turn("fact", "should_yield", true),
            turn("fact", "should_yield", true),
            turn("fact", "should_yield", false),
            turn("fact", "should_not_yield", true),
            turn("fact", "should_not_yield", false),
        ],
    );

    let output = run_pass(&path, &["--json"]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "合法文件必须退出 0；stderr={}",
        stderr_of(&output)
    );
    let report = parse_json(&output);
    let passes = report["passes"].as_array().unwrap();
    assert_eq!(passes.len(), 1, "只给了 fact 的行，只应出一组：{report}");
    let p = &passes[0];
    assert_eq!(p["pass"], json!("fact"));
    assert_eq!(p["rows"], json!(9));

    // 四条 draft 都有 verdict ⇒ judged=4；判对的是 2 条 supported ⇒ precision 2/4。
    assert_eq!(p["drafts"]["total"], json!(4));
    assert_eq!(p["drafts"]["judged"], json!(4));
    assert_eq!(p["drafts"]["positive"], json!(2));
    assert_eq!(p["drafts"]["precision"], json!(0.5), "precision 手算 2/4：{report}");
    assert_eq!(p["drafts"]["verdicts"]["supported"], json!(2));
    assert_eq!(p["drafts"]["verdicts"]["wrong_subject"], json!(1));
    assert_eq!(p["drafts"]["verdicts"]["hallucinated"], json!(1));
    assert_eq!(p["drafts"]["verdicts"]["unsupported"], Value::Null, "没出现过的 verdict 不该被补 0");

    // should_yield 3 条，其中 2 条真的产出了 ⇒ recall 2/3（不是 1）。
    assert_eq!(p["turn_expectations"]["total"], json!(5));
    assert_eq!(p["turn_expectations"]["should_yield"], json!(3));
    assert_eq!(p["turn_expectations"]["should_not_yield"], json!(2));
    assert_eq!(p["turn_expectations"]["yielded"], json!(2));
    assert_eq!(p["turn_expectations"]["false_yielded"], json!(1));
    let recall = p["turn_expectations"]["recall"].as_f64().unwrap();
    assert!((recall - 2.0 / 3.0).abs() < 1e-12, "recall 手算 2/3，实得 {recall}");
    assert_eq!(p["turn_expectations"]["false_yield"], json!(0.5));

    // 文本面同一组数字，四位小数；分母非 0 时不许出现 null。
    let text = run_pass(&path, &[]);
    assert_eq!(text.status.code(), Some(0));
    let stdout = stdout_of(&text);
    assert!(stdout.contains("precision 0.5000"), "{stdout}");
    assert!(stdout.contains("recall: 2/3 = 0.6667"), "{stdout}");
    assert!(stdout.contains("false_yield: 1/2 = 0.5000"), "{stdout}");
}

/// 判据 1b：`evidence` 为空 ⇒ 退出 2，且报**行号**与字段名。
#[test]
fn empty_evidence_fails_with_the_line_number() {
    let dir = tempfile::tempdir().unwrap();
    let mut broken = draft("fact", "supported", 200);
    broken["evidence"] = json!("");
    let path = write_gold(
        dir.path(),
        "gold.jsonl",
        &[draft("fact", "supported", 199), broken],
    );

    let output = run_pass(&path, &["--json"]);
    assert_eq!(
        output.status.code(),
        Some(2),
        "空 evidence 必须是退出 2；stdout={}",
        stdout_of(&output)
    );
    let err = stderr_of(&output);
    assert!(err.contains("line 2"), "必须报出错行号 2：{err}");
    assert!(err.contains("evidence"), "必须报出错字段 evidence：{err}");
    assert!(stdout_of(&output).trim().is_empty(), "坏文件不许出报表：{}", stdout_of(&output));
}

/// 判据 1c：verdict 不在闭集 ⇒ 退出 2（闭集是**按 pass** 各一套，不是一张大表）。
#[test]
fn a_verdict_outside_the_closed_set_fails() {
    let dir = tempfile::tempdir().unwrap();
    // `correct` 属于 time/derive 的闭集，不属于 fact：跨 pass 借用也必须拒绝。
    let path = write_gold(dir.path(), "gold.jsonl", &[draft("fact", "correct", 300)]);
    let output = run_pass(&path, &["--json"]);
    assert_eq!(output.status.code(), Some(2), "闭集外的 verdict 必须退出 2");
    let err = stderr_of(&output);
    assert!(err.contains("line 1"), "{err}");
    assert!(err.contains("verdict"), "{err}");
    assert!(err.contains("fact"), "要说出是哪一套闭集：{err}");
}

/// 判据 1d：分母为 0 的指标是 `null`，不是 0 也不是 1。
#[test]
fn zero_denominators_report_null_not_zero() {
    let dir = tempfile::tempdir().unwrap();

    // 只有 draft、没有 turn_expectation ⇒ recall / false_yield 都没有分母。
    let drafts_only = write_gold(
        dir.path(),
        "drafts.jsonl",
        &[draft("fact", "supported", 400), draft("fact", "unsupported", 401)],
    );
    let output = run_pass(&drafts_only, &["--json"]);
    assert_eq!(output.status.code(), Some(0), "stderr={}", stderr_of(&output));
    let p = parse_json(&output)["passes"][0].clone();
    assert_eq!(p["drafts"]["precision"], json!(0.5), "4xx 两条里 1 条 supported");
    assert_eq!(p["turn_expectations"]["recall"], Value::Null, "没有 should_yield ⇒ null");
    assert_eq!(
        p["turn_expectations"]["false_yield"],
        Value::Null,
        "没有 should_not_yield ⇒ null"
    );

    // 只有 turn_expectation、没有 draft ⇒ precision 没有分母。
    let turns_only = write_gold(
        dir.path(),
        "turns.jsonl",
        &[turn("fact", "should_yield", true)],
    );
    let output = run_pass(&turns_only, &["--json"]);
    assert_eq!(output.status.code(), Some(0));
    let p = parse_json(&output)["passes"][0].clone();
    assert_eq!(p["drafts"]["total"], json!(0));
    assert_eq!(p["drafts"]["judged"], json!(0));
    assert_eq!(p["drafts"]["precision"], Value::Null, "0/0 必须是 null，不是 0");
    assert_eq!(p["turn_expectations"]["recall"], json!(1.0));
    assert_eq!(
        p["turn_expectations"]["false_yield"],
        Value::Null,
        "false_yield 的 0/0 也必须是 null（不是 0.0，那会被读成'一次都没误抽'）"
    );

    // 文本面同样打 null，不是 0.0000。
    let text = stdout_of(&run_pass(&turns_only, &[]));
    assert!(text.contains("precision null"), "{text}");
    assert!(text.contains("false_yield: 0/0 = null"), "{text}");
}

/// 判据 1e：同一文件混两类 pass ⇒ 分组各出一份；`--pass` 只出指定那一组。
#[test]
fn two_passes_in_one_file_report_two_groups() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_gold(
        dir.path(),
        "gold.jsonl",
        &[
            draft("fact", "supported", 500),
            draft("fact", "hallucinated", 501),
            draft("time", "correct", 502),
            turn("time", "should_yield", false),
        ],
    );

    let report = parse_json(&run_pass(&path, &["--json"]));
    let passes = report["passes"].as_array().unwrap();
    assert_eq!(passes.len(), 2, "两类 pass 必须出两组：{report}");
    assert_eq!(passes[0]["pass"], json!("fact"));
    assert_eq!(passes[1]["pass"], json!("time"));
    assert_eq!(passes[0]["rows"], json!(2));
    assert_eq!(passes[1]["rows"], json!(2));

    // time 的"判对"是 `correct`（闭集里没有 supported）。照字面套 supported 会把它读成 0。
    assert_eq!(passes[1]["drafts"]["positive"], json!(1));
    assert_eq!(passes[1]["drafts"]["precision"], json!(1.0));
    assert_eq!(passes[1]["drafts"]["verdicts"]["correct"], json!(1));
    assert_eq!(passes[1]["turn_expectations"]["recall"], json!(0.0), "该产出却没产出 ⇒ 0");

    // 过滤器：--pass time 只剩 time 一组；--pass derive 文件里没有，也给一份全 null 的报表。
    let only_time = parse_json(&run_pass(&path, &["--json", "--pass", "time"]));
    let passes = only_time["passes"].as_array().unwrap();
    assert_eq!(passes.len(), 1);
    assert_eq!(passes[0]["pass"], json!("time"));
    assert_eq!(passes[0]["drafts"]["total"], json!(1));

    let empty = parse_json(&run_pass(&path, &["--json", "--pass", "derive"]));
    let passes = empty["passes"].as_array().unwrap();
    assert_eq!(passes.len(), 1, "指定了 pass 就必须回一份报表，不许空输出");
    assert_eq!(passes[0]["rows"], json!(0));
    assert_eq!(passes[0]["drafts"]["precision"], Value::Null);
}

/// 派发档 §二逐条点名的必填字段：`item_id`+`draft_index`（draft）、`actual_yielded`（turn）、
/// 以及 `pass` 的闭集。缺一个都要退出 2 并报行号+字段，不许静默跳过。
#[test]
fn required_provenance_fields_are_enforced() {
    let dir = tempfile::tempdir().unwrap();

    let mut no_item = draft("fact", "supported", 0);
    no_item.as_object_mut().unwrap().remove("item_id");
    let path = write_gold(dir.path(), "no_item.jsonl", &[no_item]);
    let output = run_pass(&path, &[]);
    assert_eq!(output.status.code(), Some(2));
    let err = stderr_of(&output);
    assert!(err.contains("line 1"), "{err}");
    assert!(err.contains("item_id"), "{err}");

    let mut no_draft_index = draft("fact", "supported", 600);
    no_draft_index.as_object_mut().unwrap().remove("draft_index");
    let path = write_gold(dir.path(), "no_draft_index.jsonl", &[no_draft_index]);
    let err = stderr_of(&run_pass(&path, &[]));
    assert!(err.contains("draft_index"), "{err}");

    let mut no_yielded = turn("fact", "should_yield", true);
    no_yielded.as_object_mut().unwrap().remove("actual_yielded");
    let path = write_gold(dir.path(), "no_yielded.jsonl", &[no_yielded]);
    let output = run_pass(&path, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr_of(&output).contains("actual_yielded"));

    let mut bad_pass = draft("decision", "supported", 601);
    bad_pass["pass"] = json!("decision");
    let path = write_gold(dir.path(), "bad_pass.jsonl", &[bad_pass]);
    let output = run_pass(&path, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr_of(&output).contains("pass"));

    // 文件本身不存在：同样是用法/格式错 ⇒ 2（不是 1，1 留给"读库/写盘故障"）。
    let missing = run_pass(&dir.path().join("does-not-exist.jsonl"), &[]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(stderr_of(&missing).contains("gold_unreadable"), "{}", stderr_of(&missing));
}

#[test]
fn time_draft_may_use_time_expr_id_instead_of_item_id() {
    // GOLD-SPEC §4：time/derive 的草稿不来自 review_item.payload，用 source_id 溯源。
    let dir = tempfile::tempdir().unwrap();
    let row = json!({
        "kind": "draft", "pass": "time", "family": "",
        "time_expr_id": "fda8f907acd83222d840a53d5576a280f79ce5f58ccdc4ceda2c1dab3b91a28f",
        "turn_uids": ["00000000-0000-0000-0000-000000000000"],
        "draft": { "text": "下周三之前" },
        "verdict": "correct",
        "evidence": "短语「下周三之前」，参照钟周一 ⇒ 下周三 = 11-04",
        "annotator": "time-gold", "annotated_at": "2026-10-28"
    });
    let path = write_gold(dir.path(), "time.jsonl", &[row]);
    let output = run_pass(&path, &[]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr_of(&output));
    assert!(stdout_of(&output).contains("time"), "{}", stdout_of(&output));

    // 既没有 item_id 也没有 source_id/time_expr_id ⇒ 仍然拒。
    let mut orphan = draft("time", "correct", 1);
    orphan.as_object_mut().unwrap().remove("item_id");
    let path = write_gold(dir.path(), "orphan.jsonl", &[orphan]);
    let output = run_pass(&path, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr_of(&output).contains("item_id"), "{}", stderr_of(&output));
}
