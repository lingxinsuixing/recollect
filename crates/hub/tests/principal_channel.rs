//! P0-1：导入路径上的**主人身份生产者**（`RECOLLECT_PRINCIPAL_NAME` 那一支）。
//!
//! 这一片钉四件事，四条都在真进程 + 真库上跑（`hub import` 是那条路的唯一出口）：
//! 1. 设了名字 ⇒ 本次涉及的每个会话**追加一条** claim（`source=agent_runtime`、
//!    `claimed_by` 写明是环境变量那一格）；同会话重导**不再长第二行**；落章同时把
//!    `sessions.owner_principal_id` 同步成该主人（W24a/P1-7 的镜像列）；
//! 2. 只给名字（没给 id）⇒ id 按既有的内容寻址派生，与本 scope 的 person 锚同一个算法；
//! 3. **不设环境变量 ⇒ 一格都不写**（阴性对照：claim 表与 `sessions` 归属列与 BASE 逐字相同）；
//! 4. **已有有效章 ⇒ 自动盖章不动它**（`human_claim` / `correction` 比环境变量更有分量）。
//!
//! 断言面是 transcript.db（claim 表）与 memory.db（主人锚 / mention），不是导入报告。

use rc_store::Root;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ENV_NAME: &str = "RECOLLECT_PRINCIPAL_NAME";
const ENV_ID: &str = "RECOLLECT_PRINCIPAL_ID";
const ENV_SCOPE: &str = "RECOLLECT_PRINCIPAL_SCOPE";
/// 三格环境变量都不是 `env!("…")` 能看见的编译期量，所以这里逐条清空：
/// 用例进程的 ambient 值不许影响读数（`crates/hub/tests/cycle_once.rs` 同一条做法）。
const ENV_KEYS: [&str; 3] = [ENV_NAME, ENV_ID, ENV_SCOPE];

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn write_fixture(dir: &Path, name: &str, json: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, json).unwrap();
    p
}

/// 跑一次 `hub import`。`env` 里的键先设、`ENV_KEYS` 其余一律清空（ambient 不许漏进来）。
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

fn claims(root: &Root) -> Vec<(String, String, String, Option<String>, String, String)> {
    let mut st = root
        .transcript()
        .conn()
        .prepare(
            "SELECT session_key, device_id, principal_id, principal_name, source, claimed_by
               FROM session_principal_claim ORDER BY session_key, claimed_at_ms",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn count(root: &Root, sql: &str) -> i64 {
    root.transcript().conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn session_owner(root: &Root, session: &str) -> Option<String> {
    root.transcript()
        .conn()
        .query_row(
            "SELECT owner_principal_id FROM sessions WHERE session_key=?1",
            [session],
            |r| r.get::<_, Option<String>>(0),
        )
        .unwrap()
}

/// 一条 human 轮（正文含「我」⇒ 导入末尾那一遍会写主人锚 mention），`project_hint` 定 scope。
fn one_session(session: &str) -> String {
    format!(
        r#"{{"format_version":1,"sessions":[{{"session_key":"{session}","device_id":"dev-p1","source_agent":"qoder","project_hint":"recollect","turns":[{{"role":"human","body":"我把这件事记在这儿","created_at_ms":1730000000000}}]}}]}}"#
    )
}

// ── ① 设了名字 ⇒ 追加一条章；重导不长第二行 ────────────────────────────────

#[test]
fn 环境声明的主人身份在导入时落成一条章_重导不重复() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", &one_session("ses-p1"));

    let out = import(&f, &data, &[(ENV_NAME, "示例主人"), (ENV_ID, "p-long")]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let root = Root::open(&data).unwrap();
    let rows = claims(&root);
    assert_eq!(rows.len(), 1, "一次导入恰好一条章：{rows:?}");
    let (session, device, id, name, source, claimed_by) = &rows[0];
    assert_eq!((session.as_str(), device.as_str()), ("ses-p1", "dev-p1"));
    assert_eq!(id, "p-long", "给了 RECOLLECT_PRINCIPAL_ID 就用它");
    assert_eq!(name.as_deref(), Some("示例主人"), "可读名进 principal_name");
    assert_eq!(source, "agent_runtime", "闭集第一值：Agent 运行时声明");
    assert_eq!(claimed_by, "env:RECOLLECT_PRINCIPAL_NAME", "台账里要写明章从哪来");
    // `sessions.owner_principal_id` 由 `claim_on` 同步写上（W24a/P1-7：claim 表是真相，
    // 这一列是镜像；两列长期不一致时 doctor/归属面按旧列读会误报"无主"）。
    assert_eq!(
        session_owner(&root, "ses-p1"),
        Some("p-long".into()),
        "落章必须同步会话归属镜像列"
    );

    // 重导同一份：`claim_on` 的幂等一支 + "已有有效章就跳过"双重挡住。
    let again = import(&f, &data, &[(ENV_NAME, "示例主人"), (ENV_ID, "p-long")]);
    assert_eq!(again.status.code(), Some(0), "{}", String::from_utf8_lossy(&again.stderr));
    let root = Root::open(&data).unwrap();
    assert_eq!(claims(&root).len(), 1, "重导又长了一条章：{:?}", claims(&root));
}

// ── ② 只给名字 ⇒ id 按内容寻址派生（与 person 锚同一个算法）────────────────

#[test]
fn 只给名字时_principal_id按person口径派生() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", &one_session("ses-p2"));

    let out = import(&f, &data, &[(ENV_NAME, "示例主人")]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let root = Root::open(&data).unwrap();
    let rows = claims(&root);
    assert_eq!(rows.len(), 1, "{rows:?}");
    // 派生口径：`compute_mem_id(Anchor, "person", 名字, "", 会话 scope)`。scope 是会话行
    // 落库之后的真值（这里 `project_hint='recollect'` ⇒ scope_key 就是它）。
    let expected = rc_memory::compute_mem_id(
        rc_memory::MemTable::Anchor,
        "person",
        "示例主人",
        "",
        &rc_memory::Scope {
            kind: rc_memory::ScopeKind::Project,
            key: "recollect".into(),
        },
    );
    assert_eq!(rows[0].2, expected, "派生 id 与 person 锚不是同一把尺子");
}

// ── ③ 阴性对照：不设环境变量 ⇒ claim 表与归属列与 BASE 逐字相同 ──────────────

#[test]
fn 不设环境变量时_一格都不写() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", &one_session("ses-p3"));

    let out = import(&f, &data, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let root = Root::open(&data).unwrap();
    assert_eq!(claims(&root).len(), 0, "没有声明就不该有章");
    assert_eq!(session_owner(&root, "ses-p3"), None, "回落列同样一格不写");
    assert_eq!(
        count(&root, "SELECT count(*) FROM turns WHERE session_key='ses-p3'"),
        1,
        "转录层照常收（阴性对照不许把导入也变没）"
    );
}

// ── ④ 已有有效章 ⇒ 自动盖章不动它 ─────────────────────────────────────────

#[test]
fn 已有有效章时_环境声明不覆盖它() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    // 第一份：人当场盖的章（`human_claim`，id 与名字都不是环境变量那一套）。
    let human = one_session("ses-p4").replace(
        "\"source_agent\":\"qoder\"",
        "\"source_agent\":\"qoder\",\"owner_principal_id\":\"p-human\",\"principal_name\":\"示例主人本人\",\"principal_source\":\"human_claim\"",
    );
    let f1 = write_fixture(dir.path(), "human.json", &human);
    let out1 = import(&f1, &data, &[]);
    assert_eq!(out1.status.code(), Some(0), "{}", String::from_utf8_lossy(&out1.stderr));

    // 第二份：同会话再来一轮，这次带着环境声明。**章不许被顶掉**。
    let second = r#"{"format_version":1,"sessions":[{"session_key":"ses-p4","device_id":"dev-p1","source_agent":"qoder","project_hint":"recollect","turns":[{"role":"human","body":"我再说一句","created_at_ms":1730000005000}]}]}"#;
    let f2 = write_fixture(dir.path(), "second.json", second);
    let out2 = import(&f2, &data, &[(ENV_NAME, "示例主人"), (ENV_ID, "p-env")]);
    assert_eq!(out2.status.code(), Some(0), "{}", String::from_utf8_lossy(&out2.stderr));

    let root = Root::open(&data).unwrap();
    let rows = claims(&root);
    assert_eq!(rows.len(), 1, "环境声明把人的章顶掉了：{rows:?}");
    assert_eq!(rows[0].2, "p-human", "现行章仍是人盖的那一条");
    assert_eq!(rows[0].4, "human_claim");
    // 解析读数也仍是人的那一条（`resolve_principal` 是唯一读法）。
    let resolved = rc_ledger::transcript::resolve_principal(root.transcript(), "ses-p4", "dev-p1")
        .unwrap()
        .expect("有章就该解析得出来");
    assert_eq!(resolved.principal_id, "p-human");
}

// ── ⑤ 声明 → claim → 主人锚 → mention 的整条链（同一次导入内生效）──────────

#[test]
fn 声明之后的主人锚不带设备_且human轮的我指向它() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", &one_session("ses-p5"));

    let out = import(&f, &data, &[(ENV_NAME, "示例主人"), (ENV_ID, "p-chain")]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    let root = Root::open(&data).unwrap();
    // 主人锚：kind=person、device_id IS NULL（契约）、归属行绑在 principal 上。
    let (anchor_id, canonical, device_id): (String, String, Option<String>) = root
        .memory()
        .conn()
        .query_row(
            "SELECT a.id, a.canonical, ro.device_id FROM anchor a
               JOIN resource_owner ro ON ro.mem_table='anchor' AND ro.mem_id=a.id
              WHERE a.kind='person' AND ro.owner_principal_id='p-chain'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .expect("声明之后必须建出主人锚");
    assert_eq!(device_id, None, "主人锚的 device_id 必须是 NULL（与正文人名锚的唯一区别）");
    assert_eq!(canonical, "p-chain", "canonical 恒等于 principal_id（改名不换锚）");
    // 「我」那条 mention 指向它，且它就是这一轮在说话的人。
    let (surface, speaker): (String, Option<String>) = root
        .memory()
        .conn()
        .query_row(
            "SELECT surface, speaker_anchor_id FROM mention WHERE anchor_id=?1",
            [&anchor_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("human 轮的「我」必须落一条 mention");
    assert_eq!(surface, "我");
    assert_eq!(speaker.as_deref(), Some(anchor_id.as_str()));
}
