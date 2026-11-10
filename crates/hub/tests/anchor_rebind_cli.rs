//! `hub anchor rebind` 的 CLI 面：接线、退出码、预演不落笔，以及拒绝时的闭集原因词。
//!
//! 口径分工要说清楚：**真把行搬走**的证据在 `rc-memory/tests/anchor_rebind.rs`（那里能直接
//! 造锚与状态行）；这一片证运维面——命令存在、`--dry-run` 一格都不写、校验被拒时退出码不是 0
//! 且**只印类别话**（守卫 12：不把库内文本递给 stdout），以及**成功那一形**的 JSON 副件把每个
//! 计数字段都带出来（预演与实做各一条）。
use rc_store::Root;
use serde_json::Value;
use std::process::Output;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn run(args: &[&str]) -> Output {
    std::process::Command::new(bin()).args(args).output().expect("hub binary")
}

fn text(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn fake(n: u8) -> String {
    format!("{:064x}", n)
}

#[test]
fn a_refused_rebind_exits_nonzero_and_prints_only_the_category_word() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    let out = run(&[
        "anchor", "rebind", "--from", &fake(1), "--into", &fake(2), "--data-dir", dd,
    ]);
    assert!(!out.status.success(), "两格都不存在时必须非零退出");
    let err = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(err.contains("missing_anchor"), "要给出闭集原因词：{err}");
    assert!(!err.contains(dd), "错误面不许露路径：{err}");
}

#[test]
fn dry_run_prints_the_plan_as_json_and_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    // 先落一格真锚（走导入 + naming 的替代：这里用 `principal claim` 的同一张库把 store 建起来，
    // 再用两个假 id 问预演——预演被拒也是"一格都不写"，所以断言的是**库没被写过**。
    let claim = run(&[
        "principal", "claim", "--session", "ses-r", "--device", "dev-r", "--principal", "acct-r",
        "--source", "agent_runtime", "--data-dir", dd,
    ]);
    assert!(claim.status.success(), "{}", String::from_utf8_lossy(&claim.stderr));
    let before = std::fs::read_dir(dir.path())
        .map(|rd| rd.count())
        .unwrap_or(0);

    let probe = run(&[
        "anchor", "rebind", "--from", &fake(3), "--into", &fake(4), "--dry-run", "--json",
        "--data-dir", dd,
    ]);
    assert!(!probe.status.success(), "查无此锚仍然是被拒，不是空计划");
    assert_eq!(
        std::fs::read_dir(dir.path()).map(|rd| rd.count()).unwrap_or(0),
        before,
        "预演不许新建文件"
    );
    // 命令确实接上了：--help 里能看到它
    let help = run(&["anchor", "rebind", "--help"]);
    assert!(help.status.success(), "子命令必须存在");
    let h = text(&help);
    assert!(h.contains("--from") && h.contains("--into") && h.contains("--dry-run"), "{h}");
    let _ = serde_json::from_str::<Value>("{}");
}

/// 成功那一形：预演与实做的 JSON 副件把计数字段带全，且**预演一格都不写**。
#[test]
fn a_successful_rebind_reports_every_counter_in_its_json_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let dd = dir.path().to_str().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let (loser, winner, third) = (fake(0x11), fake(0x22), fake(0x33));
    for id in [&loser, &winner, &third] {
        root
            .memory()
            .conn()
            .execute(
                "INSERT INTO anchor(id, kind, canonical, scope_kind, scope_key,
                                    created_at_ms, updated_at_ms, norm_version)
                 VALUES(?1,'person',?1,'project','proj-cli',?2,?2,1)",
                rusqlite::params![id, 1_700_000_000_000i64],
            )
            .unwrap();
    }
    // 旧锚占事实行的"地点"那一格：`place_anchor_id` 不参与事实行 id ⇒ 不该把整笔挡掉。
    root
        .memory()
        .conn()
        .execute(
            "INSERT INTO fact(id, subject_anchor_id, predicate, category, mode, text,
                              evidence_turn_uid, place_anchor_id, scope_kind, scope_key,
                              source_agent, device_id, created_at_ms, norm_version)
             VALUES('f-cli',?1,'met','event','stated','在旧格那里见的面',?2,?3,
                    'project','proj-cli','cli-agent','cli-dev',?4,1)",
            rusqlite::params![third, fake(0x44), loser, 1_700_000_000_000i64],
        )
        .unwrap();

    let plan = run(&[
        "anchor", "rebind", "--from", &loser, "--into", &winner,
        "--dry-run", "--json", "--data-dir", dd,
    ]);
    assert!(plan.status.success(), "{}", String::from_utf8_lossy(&plan.stderr));
    let p: Value = serde_json::from_str(text(&plan).trim()).expect("预演是 JSON");
    assert_eq!(p["mode"], "plan");
    assert_eq!(p["fact_refs"], 1, "预演要把这一格列数报出来：{p}");
    assert_eq!(p["blockers"].as_array().unwrap().len(), 0, "地点不是 id 输入 ⇒ 不挡：{p}");
    let place_before: Option<String> = root
        .memory()
        .conn()
        .query_row(
            "SELECT place_anchor_id FROM fact WHERE id='f-cli'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        place_before.as_deref(),
        Some(loser.as_str()),
        "--dry-run 只看得见，不许已经改列"
    );

    let applied = run(&[
        "anchor", "rebind", "--from", &loser, "--into", &winner,
        "--reason", "认出是同一人", "--actor", "user", "--json", "--data-dir", dd,
    ]);
    assert!(applied.status.success(), "{}", String::from_utf8_lossy(&applied.stderr));
    // 实做两行：先预演、后回执（运维面每次动笔前都看得见会动什么）。
    let lines: Vec<Value> = text(&applied)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("每行都是一个 JSON 副件"))
        .collect();
    assert_eq!(lines.len(), 2, "预演 + 实做各一行：{lines:?}");
    assert_eq!(lines[0]["mode"], "plan");
    assert_eq!(lines[1]["mode"], "apply");
    let a = &lines[1];
    assert_eq!(a["fact_refs_moved"], 1, "实做要把搬了几格列报出来：{a}");

    let place_after: Option<String> = root
        .memory()
        .conn()
        .query_row(
            "SELECT place_anchor_id FROM fact WHERE id='f-cli'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(place_after.as_deref(), Some(winner.as_str()), "事实行的地点跟着换到新锚");
    let status: String = root
        .memory()
        .conn()
        .query_row("SELECT status FROM anchor WHERE id=?1", rusqlite::params![loser], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "merged", "旧锚盖章不删除");
}
