//! W24c：`hub issue list|resolve` 的黑盒面（P1-9 的运维出口）。
//!
//! 为什么这一面必须存在：`rc_memory::issues::resolve` 早就有了，但此前唯一的调用者是
//! `hub derive retry`（只关 `pass='derive'` 的隔离行）。现网 54 条 `unresolved_surface`
//! （命名 pass 对 commit hash / 路径 / 通用平台名的**正确自拒**）因此没有任何关闭路径，
//! `doctor` 的 `issues.unresolved_surface` 永久红。这里钉四条：
//! 1. `list` 默认列全部（含已解决），`--json` 的格子与 `detail` 截断是契约；
//! 2. `resolve` 真把那一行写成 `resolved_at_ms`，且 `--open` 之后只剩另一半；
//! 3. 已解决的行再关一次仍是成功（幂等，0 行才是"id 不存在"）；
//! 4. 不存在的 id ⇒ 退出 3（运维错误），不是一条 SQL 故障也不是一串 0。
use rc_memory::issues;
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const NOW: i64 = 1_700_000_000_000;

fn bin() -> &'static str { env!("CARGO_BIN_EXE_recollect-hub") }

fn data_dir(dir: &Path) -> PathBuf { dir.join("data") }

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .arg("--data-dir")
        .arg(data_dir(dir))
        .output()
        .expect("hub binary")
}

fn code(out: &Output) -> i32 { out.status.code().unwrap() }

fn stdout_json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 值")
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 两条审计行：一条 naming 的悬空桶（detail 长到必须截断）+ 一条 derive 的隔离行。
/// 走 `issues::record` 那个唯一写口，不手写 INSERT——这一面读的就是生产写出来的行。
fn seed(data: &Path) -> (i64, i64) {
    let root = Root::open(data).unwrap();
    let long = format!("{{\"surface\":\"{}\",\"why\":\"commit hash\"}}", "x".repeat(300));
    let naming = issues::record(
        root.memory(),
        "naming",
        issues::UNRESOLVED_SURFACE_CODE,
        "run-naming-1",
        &long,
        NOW,
    )
    .unwrap();
    let derive = issues::record(
        root.memory(),
        "derive",
        issues::DERIVE_SCHEMA_CODE,
        "run-derive-1",
        "{\"why\":\"bad json\"}",
        NOW,
    )
    .unwrap();
    (naming, derive)
}

#[test]
fn list_shows_every_issue_and_resolve_closes_exactly_one() {
    let dir = tempfile::tempdir().unwrap();
    let (naming, derive) = seed(&data_dir(dir.path()));

    let out = run(dir.path(), &["issue", "list", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    let arr = v.as_array().expect("`issue list --json` 是数组");
    assert_eq!(arr.len(), 2, "默认列全部（含已解决）：{v}");
    assert_eq!(arr[0]["id"], json!(naming), "{v}");
    assert_eq!(arr[0]["pass"], json!("naming"));
    assert_eq!(arr[0]["code"], json!("unresolved_surface"));
    assert_eq!(arr[0]["resolved_at_ms"], Value::Null);
    assert_eq!(arr[1]["id"], json!(derive));
    // `detail` 折成单行 + 截断到 120 字符（+ 一个省略号）：列表不是正文阅读器。
    let detail = arr[0]["detail"].as_str().unwrap();
    assert!(detail.ends_with('…'), "长 detail 必须截断：{detail}");
    assert_eq!(detail.chars().count(), 121, "120 字符 + 省略号：{detail}");
    assert!(!detail.contains('\n'), "折成单行：{detail}");

    // 关闭第一条：JSON 读数 + 库面状态（两行各归各）。
    let id = naming.to_string();
    let out = run(dir.path(), &["issue", "resolve", "--id", &id, "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["id"], json!(naming));
    assert_eq!(v["resolved"], json!(true));
    assert!(v["resolved_at_ms"].as_i64().unwrap() > NOW, "{v}");
    {
        let root = Root::open(&data_dir(dir.path())).unwrap();
        assert!(issues::get(root.memory(), naming).unwrap().unwrap().resolved_at_ms.is_some());
        assert!(issues::get(root.memory(), derive).unwrap().unwrap().resolved_at_ms.is_none());
    }

    // `--open` 只剩另一条；文本面同一份读数（一行一条）。
    let out = run(dir.path(), &["issue", "list", "--open", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v.as_array().unwrap().len(), 1, "只有一条未解决：{v}");
    assert_eq!(v[0]["id"], json!(derive));
    let out = run(dir.path(), &["issue", "list", "--open"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains(&format!("id={derive} ")), "{text}");
    assert!(!text.contains(&format!("id={naming} ")), "已解决的不进 --open：{text}");
    assert!(text.contains("resolved_at_ms=null"), "{text}");

    // 幂等：已解决的行再关一次仍成功（`issues::resolve` 命中 1 行 ⇒ true）。
    let again = run(dir.path(), &["issue", "resolve", "--id", &id]);
    assert_eq!(code(&again), 0, "{}", stderr_text(&again));
}

#[test]
fn code_and_pass_filters_narrow_the_list() {
    let dir = tempfile::tempdir().unwrap();
    let (naming, _) = seed(&data_dir(dir.path()));

    let out = run(dir.path(), &["issue", "list", "--code", "unresolved_surface", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v.as_array().unwrap().len(), 1, "code 过滤：{v}");
    assert_eq!(v[0]["id"], json!(naming));

    let out = run(dir.path(), &["issue", "list", "--pass", "derive", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v.as_array().unwrap().len(), 1, "pass 过滤：{v}");
    assert_eq!(v[0]["pass"], json!("derive"));

    // 两个筛选同时给 = 交集（没有 naming+derive 的行 ⇒ 空数组，不是错误）。
    let out = run(dir.path(), &["issue", "list", "--code", "schema", "--pass", "naming", "--json"]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(stdout_json(&out), json!([]));
}

#[test]
fn resolving_a_missing_id_is_an_operational_error() {
    let dir = tempfile::tempdir().unwrap();
    seed(&data_dir(dir.path()));
    let out = run(dir.path(), &["issue", "resolve", "--id", "9999", "--json"]);
    assert_eq!(code(&out), 3, "不存在的 id 是运维错误：{}", stderr_text(&out));
    assert!(stderr_text(&out).contains("no such issue"), "{}", stderr_text(&out));
    assert!(
        !stderr_text(&out).contains("invalid invocation"),
        "这不是用法错误（参数形状是好的，只是 id 不存在）：{}",
        stderr_text(&out)
    );
    assert!(out.stdout.is_empty(), "失败不许在 stdout 交一份读数");
}

#[test]
fn list_on_a_missing_store_exits_three_without_creating_it() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    let out = Command::new(bin())
        .args(["issue", "list", "--json"])
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert_eq!(code(&out), 3, "读不出来必须是运维失败：{}", stderr_text(&out));
    assert!(!data.exists(), "只读装配不许把库建出来（`Root::open` 的迁移副作用会在这里现形）");
}
