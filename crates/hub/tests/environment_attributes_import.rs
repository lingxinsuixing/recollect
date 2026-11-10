//! W14c：导入路径上的**环境/账号属性生产者**（`RECOLLECT_ENV_*` / `RECOLLECT_ACCOUNT_*`）。
//!
//! 真进程 + 真库（照 `principal_channel.rs` 的跑法），断言面是记忆库的 `anchor` / `attribute` /
//! `resource_owner` 三张表——不是导入报告。
//!
//! 五条：
//! 1. 五格环境变量给全 ⇒ **一个 `object` 锚**（canonical=主机名、归属=主人、带设备）
//!    + **六条属性**（主机名 + 品牌/OS 账号/B 站昵称/Qoder 用户名 + 设备标识）；
//! 2. 重导同一 JSON + 同一组环境变量 ⇒ 行数不变（锚与属性都由内容寻址 id 兜住）；
//! 3. 只给主机名 ⇒ 环境锚照建、属性只有设备标识那一条（其余四格没有落点就不写）；
//! 4. **没有主人 ⇒ 一格都不写**（没有章的会话造不出"有主的环境"——那正是 W14b 要消掉的形状）；
//! 5. **阴性对照**：五格环境变量都不设 ⇒ 环境锚 0 个、属性 0 行（只留下主人锚那一条既有产物）。
use rc_store::Root;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ENV_HOSTNAME: &str = "RECOLLECT_ENV_HOSTNAME";
const ENV_BRAND: &str = "RECOLLECT_ENV_BRAND";
const ENV_OS_USER: &str = "RECOLLECT_ENV_OS_USER";
const ENV_BILIBILI: &str = "RECOLLECT_ACCOUNT_BILIBILI";
const ENV_QODER: &str = "RECOLLECT_ACCOUNT_QODER";
const ENV_PRINCIPAL_NAME: &str = "RECOLLECT_PRINCIPAL_NAME";
const ENV_PRINCIPAL_ID: &str = "RECOLLECT_PRINCIPAL_ID";

/// 八格都逐条清空：用例进程的 ambient 值不许影响读数（`principal_channel.rs` 同一条做法）。
const ENV_KEYS: [&str; 7] = [
    ENV_HOSTNAME, ENV_BRAND, ENV_OS_USER, ENV_BILIBILI, ENV_QODER, ENV_PRINCIPAL_NAME,
    ENV_PRINCIPAL_ID,
];

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn write_fixture(dir: &Path, name: &str, json: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, json).unwrap();
    p
}

/// 跑一次 `hub import`。`env` 里的键先设、`ENV_KEYS` 其余一律清空。
fn import(fixture: &Path, data_dir: &Path, env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.args(["import", fixture.to_str().unwrap(), "--data-dir"])
        .arg(data_dir)
        .arg("--json");
    for key in ENV_KEYS {
        cmd.env_remove(key);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().unwrap()
}

/// 一条 human 轮（不写「我」也行：本片只看环境锚与属性），`project_hint` 定 scope。
fn one_session(session: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"local-qoder","source_agent":"qoder","project_hint":"recollect","turns":[{{"role":"human","body":"这台机器就是现网那台","created_at_ms":1730000000000}}]}}]}}"#
    )
}

/// 现网那五个值（用户 2026-11-06 给的名字表）。
fn full_env() -> Vec<(&'static str, &'static str)> {
    vec![
        (ENV_PRINCIPAL_NAME, "示例主人"),
        (ENV_PRINCIPAL_ID, "示例主人"),
        (ENV_HOSTNAME, "example-host"),
        (ENV_BRAND, "Acer"),
        (ENV_OS_USER, "u"),
        (ENV_BILIBILI, "甲某随行"),
        (ENV_QODER, "example-account"),
    ]
}

fn count(root: &Root, sql: &str) -> i64 {
    root.memory().conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

/// `(key, value_text, mode, source_agent, device_id)` 按值排序。
fn attributes(root: &Root) -> Vec<(String, String, String, String, String)> {
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT key, value_text, mode, source_agent, device_id FROM attribute
              ORDER BY key, value_text",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn env_anchor(root: &Root) -> (String, String) {
    root.memory()
        .conn()
        .query_row(
            "SELECT id, canonical FROM anchor WHERE kind='object' AND canonical='example-host'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
}

// ── ① 五格给全 ⇒ 一个环境锚 + 五条属性，归属是主人 ──────────────────────

#[test]
fn 环境声明在导入时落成环境锚与六条属性_重导不增行() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", &one_session("w14c-s1"));
    let (out, v) = {
        let out = import(&f, &data, &full_env());
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
        (out, v)
    };
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(v["turns_accepted"], 1);

    let root = Root::open(&data).unwrap();
    let (anchor_id, canonical) = env_anchor(&root);
    assert_eq!(canonical, "example-host", "环境锚以主机名为名");
    let rows = attributes(&root);
    let seen: Vec<(&str, &str)> = rows.iter().map(|r| (r.0.as_str(), r.1.as_str())).collect();
    assert_eq!(
        seen,
        vec![
            ("bilibili_account", "甲某随行"),
            ("brand", "Acer"),
            ("device_id", "local-qoder"),
            ("hostname", "example-host"),
            ("os_user", "u"),
            ("qoder_account", "example-account"),
        ],
        "声明四格 + 主机名 + 设备标识；属性值原样，不归一成别的字"
    );
    assert!(rows.iter().all(|r| r.2 == "stated"), "环境变量就是直陈");
    assert!(rows.iter().all(|r| r.3 == "qoder" && r.4 == "local-qoder"), "批次三元组进每一行");
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
    assert_eq!(principal.as_deref(), Some("示例主人"), "环境的名字归主人");
    assert_eq!(device.as_deref(), Some("local-qoder"), "环境锚带设备");

    // 重导：同一 JSON、同一组环境变量 ⇒ 行数与内容和第一次逐字相同。
    let again = import(&f, &data, &full_env());
    assert_eq!(again.status.code(), Some(0), "{}", String::from_utf8_lossy(&again.stderr));
    let root = Root::open(&data).unwrap();
    assert_eq!(count(&root, "SELECT count(*) FROM attribute"), 6, "重导不长第二行");
    assert_eq!(
        count(&root, "SELECT count(*) FROM anchor WHERE kind='object' AND canonical='example-host'"),
        1,
        "重导不长第二格锚"
    );
    assert_eq!(attributes(&root), rows, "重导不改正已有的格");
}

// ── ② 只给主机名 ⇒ 锚照建，属性只有主机名与设备标识两条 ─────────────────

#[test]
fn 只给主机名时属性只有主机名与设备标识() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", &one_session("w14c-s2"));
    let out = import(
        &f,
        &data,
        &[(ENV_PRINCIPAL_NAME, "示例主人"), (ENV_PRINCIPAL_ID, "示例主人"), (ENV_HOSTNAME, "example-host")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let root = Root::open(&data).unwrap();
    env_anchor(&root);
    let seen: Vec<(String, String)> =
        attributes(&root).into_iter().map(|r| (r.0, r.1)).collect();
    assert_eq!(
        seen,
        vec![
            ("device_id".to_string(), "local-qoder".to_string()),
            ("hostname".to_string(), "example-host".to_string()),
        ],
        "主机名既是锚名也是属性；其余四格没给就不写（不写空值）"
    );
}

// ── ③ 没有主人 ⇒ 一格都不写 ─────────────────────────────────────────────

#[test]
fn 没有主人章时环境声明一格都不写() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", &one_session("w14c-s3"));
    // 只给环境、不给主人：环境有自己的名字，但没有"它的信息归谁"。
    let out = import(&f, &data, &[(ENV_HOSTNAME, "example-host"), (ENV_BILIBILI, "甲某随行")]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let root = Root::open(&data).unwrap();
    assert_eq!(count(&root, "SELECT count(*) FROM attribute"), 0, "没有主人就没有落点");
    assert_eq!(
        count(&root, "SELECT count(*) FROM anchor WHERE kind='object' AND canonical='example-host'"),
        0,
        "不造无主的环境锚"
    );
}

// ── ④ 阴性对照：五格都不设 ⇒ 与 BASE 逐字相同（零属性、零环境锚） ────────

#[test]
fn 不设环境变量时零属性零环境锚() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", &one_session("w14c-s4"));
    // 只给主人（证明这一片的阴性对照不是"因为连主人都没有才空"）。
    let out = import(
        &f,
        &data,
        &[(ENV_PRINCIPAL_NAME, "示例主人"), (ENV_PRINCIPAL_ID, "示例主人")],
    );
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let root = Root::open(&data).unwrap();
    assert_eq!(count(&root, "SELECT count(*) FROM attribute"), 0, "env 缺席 ⇒ 属性零行");
    assert_eq!(
        count(&root, "SELECT count(*) FROM anchor WHERE kind='object'"),
        0,
        "env 缺席 ⇒ 环境锚零格"
    );
    // 主人那一半照旧（W14b 的产物）：本片的缺席不影响它。
    assert_eq!(
        count(&root, "SELECT count(*) FROM anchor WHERE kind='person' AND canonical='示例主人'"),
        1
    );
}
