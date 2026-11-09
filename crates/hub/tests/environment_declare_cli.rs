//! W24c：`hub environment declare` 的黑盒面（P1-8 的运维出口）。
//!
//! 这一面修的是一条"代码有、数据没有"的缺口：`attribute` 的写入路径只存在于导入声明
//! （v2/v3 的 environment 那一格），而现网是 v1 导入 ⇒ 表里 0 行，W14c 裁定的
//! "账号/主机/品牌走环境属性"一条也读不到。真进程 + 真库，断言面是 `anchor` /
//! `attribute` / `resource_owner` 三张表与那份 JSON 读数。
//!
//! 五条：
//! 1. 六个键给全 ⇒ 一个 `object` 锚（canonical=主机名）+ 6 行属性；owner 行的
//!    `owner_principal_id` 是转录章里那一位、`device_id` 非 NULL（NULL 是主人锚的契约）；
//! 2. 同值重跑 ⇒ 只有 `identical` 增，行数不变、锚仍一格；
//! 3. 未知键（旗标面之外）⇒ 用法错误（退出 3、`invalid invocation`），不碰库；
//! 4. principal 解析不到 ⇒ 退出 3，一格都不写；显式 `--principal` 则不查转录；
//! 5. 缺 `--principal` 时读的是**最新**那条章（不是随便一条）。
use rc_store::Root;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    serde_json::from_slice(&out.stdout).expect("stdout 必须是单个 JSON 对象")
}

fn stderr_text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

/// 走运维面盖一章（`hub principal claim`）：declare 的缺省归属从这张表的最新一条读。
/// 章的时刻是 CLI 自己的钟（`principal claim` 没有 `--now-ms`），同毫秒时按 rowid 定先后。
fn claim(dir: &Path, session: &str, principal: &str) {
    let out = Command::new(bin())
        .args([
            "principal", "claim", "--session", session, "--device", "local-qoder",
            "--principal", principal, "--source", "agent_runtime",
        ])
        .arg("--data-dir")
        .arg(data_dir(dir))
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "夹具盖章失败：{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn count(root: &Root, sql: &str) -> i64 {
    root.memory().conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// `(key, value_text, source_agent, device_id)`，按 key 排序。
fn attributes(root: &Root) -> Vec<(String, String, String, String)> {
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT key, value_text, source_agent, device_id FROM attribute
              ORDER BY key, value_text",
        )
        .unwrap();
    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

fn full_declare(dir: &Path) -> Output {
    run(dir, &[
        "environment", "declare", "--hostname", "w24c-host",
        "--brand", "Acer", "--os-user", "u",
        "--bilibili-account", "甲某随行", "--qoder-account", "example-account",
        "--json",
    ])
}

#[test]
fn declare_writes_six_attributes_and_an_owned_anchor() {
    let dir = tempfile::tempdir().unwrap();
    claim(dir.path(), "w24c-s1", "示例主人");
    let out = full_declare(dir.path());
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let v = stdout_json(&out);
    assert_eq!(v["inserted"], json!(6), "六格都给 ⇒ 6 行：{v}");
    assert_eq!(v["identical"], json!(0), "{v}");
    assert_eq!(v["reopened"], json!(0), "{v}");
    assert_eq!(v["superseded"], json!(0), "{v}");
    let anchor_id = v["anchor_id"].as_str().expect("锚 id 必须回给调用方").to_string();

    let root = Root::open(&data_dir(dir.path())).unwrap();
    let rows = attributes(&root);
    let seen: Vec<(&str, &str)> =
        rows.iter().map(|r| (r.0.as_str(), r.1.as_str())).collect();
    assert_eq!(seen, vec![
        ("bilibili_account", "甲某随行"),
        ("brand", "Acer"),
        ("device_id", "local-qoder"),
        ("hostname", "w24c-host"),
        ("os_user", "u"),
        ("qoder_account", "example-account"),
    ], "主机名兼锚名，设备标识取批次键；其余四格原样");
    assert!(rows.iter().all(|r| r.2 == "operator"), "缺省署名是 operator：{rows:?}");
    assert!(rows.iter().all(|r| r.3 == "local-qoder"), "缺省设备键：{rows:?}");

    let (kind, canonical): (String, String) = root
        .memory()
        .conn()
        .query_row("SELECT kind, canonical FROM anchor WHERE id=?1", [&anchor_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(kind, "object", "环境是东西，不是人");
    assert_eq!(canonical, "w24c-host");

    let (principal, device): (Option<String>, Option<String>) = root
        .memory()
        .conn()
        .query_row(
            "SELECT owner_principal_id, device_id FROM resource_owner
              WHERE mem_table='anchor' AND mem_id=?1",
            [&anchor_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(principal.as_deref(), Some("示例主人"), "环境的名字归转录章里那一位");
    assert_eq!(
        device.as_deref(),
        Some("local-qoder"),
        "环境锚的归属行必须带设备：NULL 是主人锚的契约（`session_principal_anchor_id` 靠它区分）"
    );
}

#[test]
fn rerunning_the_same_values_only_increments_identical() {
    let dir = tempfile::tempdir().unwrap();
    claim(dir.path(), "w24c-s2", "示例主人");
    let first = full_declare(dir.path());
    assert_eq!(code(&first), 0, "{}", stderr_text(&first));
    let anchor_id = stdout_json(&first)["anchor_id"].as_str().unwrap().to_string();

    let again = full_declare(dir.path());
    assert_eq!(code(&again), 0, "{}", stderr_text(&again));
    let v = stdout_json(&again);
    assert_eq!(v["inserted"], json!(0), "同值重跑不新增行：{v}");
    assert_eq!(v["identical"], json!(6), "只有 identical 增：{v}");
    assert_eq!(v["reopened"], json!(0), "{v}");
    assert_eq!(v["superseded"], json!(0), "{v}");
    assert_eq!(v["anchor_id"].as_str().unwrap(), anchor_id, "锚 id 内容寻址 ⇒ 同一格");

    let root = Root::open(&data_dir(dir.path())).unwrap();
    assert_eq!(count(&root, "SELECT count(*) FROM attribute"), 6, "行数不变");
    assert_eq!(
        count(&root, "SELECT count(*) FROM anchor WHERE kind='object' AND canonical='w24c-host'"),
        1,
        "锚不长大二格"
    );
}

#[test]
fn an_unknown_attribute_flag_is_a_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    // 六键闭集在旗标面上就是结构性的：拼不出第七个键 ⇒ clap 的用法错误（退出 3）。
    let out = Command::new(bin())
        .args(["environment", "declare", "--hostname", "w24c-host", "--favourite-colour", "blue"])
        .arg("--data-dir")
        .arg(&data)
        .output()
        .unwrap();
    assert_eq!(code(&out), 3, "{}", stderr_text(&out));
    assert!(stderr_text(&out).contains("invalid invocation"), "{}", stderr_text(&out));
    assert!(!data.exists(), "用法错误不碰库");
}

#[test]
fn an_unresolvable_principal_exits_three_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let data = data_dir(dir.path());
    // 空库：没有章，也没有会话。缺省解析必须拒绝，而不是造一个无主的环境锚。
    Root::open(&data).unwrap();
    let out = run(dir.path(), &["environment", "declare", "--hostname", "w24c-host", "--json"]);
    assert_eq!(code(&out), 3, "解析不到主人是运维错误：{}", stderr_text(&out));
    assert!(stderr_text(&out).contains("--principal"), "要把补法说给操作员：{}", stderr_text(&out));
    {
        let root = Root::open(&data).unwrap();
        assert_eq!(count(&root, "SELECT count(*) FROM attribute"), 0, "一格都不写");
        assert_eq!(
            count(&root, "SELECT count(*) FROM anchor WHERE kind='object'"),
            0,
            "不造无主的环境锚"
        );
    }

    // 显式 `--principal` 不查转录：同一台环境照样能声明（只有主机名 + 设备标识两行）。
    let out = run(dir.path(), &[
        "environment", "declare", "--hostname", "w24c-host", "--principal", "示例主人", "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    assert_eq!(stdout_json(&out)["inserted"], json!(2), "只给了主机名 ⇒ 主机名 + 设备标识");
}

#[test]
fn the_default_principal_comes_from_the_latest_claim() {
    let dir = tempfile::tempdir().unwrap();
    claim(dir.path(), "w24c-s5a", "甲主人");
    claim(dir.path(), "w24c-s5b", "乙主人");
    let out = run(dir.path(), &[
        "environment", "declare", "--hostname", "w24c-host", "--json",
    ]);
    assert_eq!(code(&out), 0, "{}", stderr_text(&out));
    let anchor_id = stdout_json(&out)["anchor_id"].as_str().unwrap().to_string();
    let root = Root::open(&data_dir(dir.path())).unwrap();
    let principal: Option<String> = root
        .memory()
        .conn()
        .query_row(
            "SELECT owner_principal_id FROM resource_owner
              WHERE mem_table='anchor' AND mem_id=?1",
            [&anchor_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        principal.as_deref(),
        Some("乙主人"),
        "缺省归属取最新一条章（后盖的那一位），不是随便一条"
    );
}
