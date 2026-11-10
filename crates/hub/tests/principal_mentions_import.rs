//! D31 E2E：`hub import` 提交之后的那一遍「我 / 我们」→ 主人锚 mention。
//!
//! 真进程 + 真库（照 `import_cli.rs` / `purge_cli.rs` 的跑法），断言面是记忆库的 `mention`
//! 表与 `memory_mention` 表——不是导入报告（报告只说计数，不许带正文）。
//!
//! 四条：
//! 1. human「我……」⇒ `mention` 恰好 1 行、surface='我'、指向 kind='person' 的主人锚，
//!    `memory_mention` 仍为 0（它不是这条规则的判据）；
//! 2. 重导同一 JSON ⇒ mention 总数不变；
//! 3. agent 轮的「我」⇒ 不产生新行；
//! 4. 无 claim 的回落列（手改 `sessions.owner_principal_id`）⇒ 0 行（claim 守卫）；
//! 5. 墓碑：purge 之后重导 ⇒ 不给已删会话长 mention。
use rc_store::Root;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hub")
}

fn write_fixture(dir: &Path, name: &str, body_json: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, body_json).unwrap();
    p
}

fn import(fixture: &Path, data_dir: &Path) -> (Output, serde_json::Value) {
    let out = Command::new(bin())
        .args([
            "import",
            fixture.to_str().unwrap(),
            "--data-dir",
            data_dir.to_str().unwrap(),
            "--json",
        ])
        .output()
        .unwrap();
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    (out, v)
}

/// 记忆库 `mention` 的断言面：`(surface, anchor_id, discourse_role, resolution, speaker_anchor_id)`。
fn mentions(root: &Root) -> Vec<(String, String, String, String, Option<String>)> {
    let mut st = root
        .memory()
        .conn()
        .prepare(
            "SELECT surface, anchor_id, discourse_role, resolution, speaker_anchor_id
               FROM mention ORDER BY surface",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

fn count(root: &Root, sql: &str) -> i64 {
    root.memory().conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

fn mention_turn_uids(root: &Root) -> Vec<String> {
    let mut st = root
        .memory()
        .conn()
        .prepare("SELECT turn_uid FROM mention ORDER BY turn_uid")
        .unwrap();
    st.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

// 身份三格随会话透传：导入侧在同一事务里追加 claim（`principal_source` 缺省 agent_runtime）。
const ONE_HUMAN_ME: &str = r#"{"format_version":1,"sessions":[{"session_key":"ses-d31","device_id":"dev-d31","source_agent":"qoder","owner_principal_id":"p-d31","principal_name":"某人","principal_source":"agent_runtime","turns":[{"role":"human","body":"我下周三把报价单发给你","created_at_ms":1730000000000}]}]}"#;

#[test]
fn human的我_导入后落一条指向person主人锚的mention() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", ONE_HUMAN_ME);
    let (out, v) = import(&f, &data);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(v["turns_accepted"], 1);

    let root = Root::open(&data).unwrap();
    let rows = mentions(&root);
    assert_eq!(rows.len(), 1, "一条 human 轮的「我」恰好一行：{rows:?}");
    let (surface, anchor_id, role, resolution, speaker) = &rows[0];
    assert_eq!(surface, "我");
    assert_eq!(role, "speaker");
    assert_eq!(resolution, "resolved");
    assert_eq!(speaker.as_deref(), Some(anchor_id.as_str()), "「我」就是主人锚在说");
    let kind: String = root
        .memory()
        .conn()
        .query_row("SELECT kind FROM anchor WHERE id=?1", [anchor_id], |r| r.get(0))
        .unwrap();
    assert_eq!(kind, "person", "mention 指向 kind=person 的主人锚");
    assert_eq!(
        count(&root, "SELECT count(*) FROM memory_mention"),
        0,
        "memory_mention 是「记忆行 → 锚」的派生表，不是这条规则的判据"
    );
}

#[test]
fn 重导同一json_mention总数不变() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let f = write_fixture(dir.path(), "one.json", ONE_HUMAN_ME);
    let (out1, _) = import(&f, &data);
    assert_eq!(out1.status.code(), Some(0), "{}", String::from_utf8_lossy(&out1.stderr));
    let (out2, v2) = import(&f, &data);
    assert_eq!(out2.status.code(), Some(0), "{}", String::from_utf8_lossy(&out2.stderr));
    assert_eq!(v2["turns_accepted"], 0);
    assert_eq!(v2["skipped_duplicate"], 1);

    let root = Root::open(&data).unwrap();
    assert_eq!(mentions(&root).len(), 1, "重导幂等：内容寻址 + INSERT OR IGNORE");
}

#[test]
fn agent轮的我不产生mention() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let body = r#"{"format_version":1,"sessions":[{"session_key":"ses-ag","device_id":"dev-ag","source_agent":"qoder","owner_principal_id":"p-d31","principal_name":"某人","turns":[{"role":"human","body":"我先把结论放这","created_at_ms":1730000001000},{"role":"agent","body":"我会转告团队","created_at_ms":1730000002000}]}]}"#;
    let f = write_fixture(dir.path(), "agent.json", body);
    let (out, v) = import(&f, &data);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(v["turns_accepted"], 2);

    let root = Root::open(&data).unwrap();
    let rows = mentions(&root);
    assert_eq!(rows.len(), 1, "只有 human 轮的「我」落 mention：{rows:?}");
    assert_eq!(rows[0].0, "我");
}

#[test]
fn 只有回落列没有claim_不写mention() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let first = r#"{"format_version":1,"sessions":[{"session_key":"ses-fb","device_id":"dev-fb","source_agent":"qoder","turns":[{"role":"human","body":"先交代一下背景","created_at_ms":1730000003000}]}]}"#;
    let f1 = write_fixture(dir.path(), "first.json", first);
    let (out1, _) = import(&f1, &data);
    assert_eq!(out1.status.code(), Some(0), "{}", String::from_utf8_lossy(&out1.stderr));

    // 运维手填回落列：没有 claim 行，`resolve_principal` 只能读到「没盖章」的那一半。
    {
        let root = Root::open(&data).unwrap();
        let n = root
            .transcript()
            .conn()
            .execute(
                "UPDATE sessions SET owner_principal_id='p-ghost' WHERE session_key='ses-fb' AND device_id='dev-fb'",
                [],
            )
            .unwrap();
        assert_eq!(n, 1);
        let claims: i64 = root
            .transcript()
            .conn()
            .query_row(
                "SELECT count(*) FROM session_principal_claim WHERE session_key='ses-fb'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(claims, 0, "夹具前提：这个会话没有 claim");
    }

    // 同会话第二条 human 轮，仍不带身份三格。「我」必须在 claim 守卫处被挡下。
    let second = r#"{"format_version":1,"sessions":[{"session_key":"ses-fb","device_id":"dev-fb","source_agent":"qoder","turns":[{"role":"human","body":"我明天再来","created_at_ms":1730000004000}]}]}"#;
    let f2 = write_fixture(dir.path(), "second.json", second);
    let (out2, v2) = import(&f2, &data);
    assert_eq!(out2.status.code(), Some(0), "{}", String::from_utf8_lossy(&out2.stderr));
    assert_eq!(v2["turns_accepted"], 1, "转录层照常收这一轮");

    let root = Root::open(&data).unwrap();
    assert_eq!(
        count(&root, "SELECT count(*) FROM mention"),
        0,
        "只有回落列、没有 claim ⇒ 不写 mention（否则会放宽闸 6c 第 3 支）"
    );
}

#[test]
fn 墓碑挡住的会话重导_不长mention() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let first = r#"{"format_version":1,"sessions":[{"session_key":"ses-t","device_id":"dev-t","source_agent":"qoder","owner_principal_id":"p-d31","principal_name":"某人","turns":[{"role":"human","body":"我先把资料给你","created_at_ms":1730000005000}]}]}"#;
    let f1 = write_fixture(dir.path(), "tomb-1.json", first);
    let (out1, _) = import(&f1, &data);
    assert_eq!(out1.status.code(), Some(0), "{}", String::from_utf8_lossy(&out1.stderr));
    let before = {
        let root = Root::open(&data).unwrap();
        assert_eq!(count(&root, "SELECT count(*) FROM mention"), 1);
        mention_turn_uids(&root)
    };

    let out = Command::new(bin())
        .args([
            "purge",
            "--session-key",
            "ses-t",
            "--confirm",
            "--data-dir",
            data.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    // 同会话新的一轮（带「我」）。转录层被墓碑挡住，mention 这一层也必须挡住。
    let second = r#"{"format_version":1,"sessions":[{"session_key":"ses-t","device_id":"dev-t","source_agent":"qoder","owner_principal_id":"p-d31","principal_name":"某人","turns":[{"role":"human","body":"我明天再补一份","created_at_ms":1730000006000}]}]}"#;
    let f2 = write_fixture(dir.path(), "tomb-2.json", second);
    let (out2, v2) = import(&f2, &data);
    assert_eq!(out2.status.code(), Some(0), "{}", String::from_utf8_lossy(&out2.stderr));
    assert_eq!(v2["turns_accepted"], 0, "墓碑挡住转录行");
    assert_eq!(v2["tombstoned"], 1);

    let root = Root::open(&data).unwrap();
    let after = mention_turn_uids(&root);
    // 不断"总数必须还是 1"——那会绑死「purge 不清 memory」这个现状。只断"没有新的轮
    // 被写进来"：新轮若绕过墓碑会带一个 before 里没有的 turn_uid。
    assert!(
        after.iter().all(|uid| before.contains(uid)),
        "重导给已删会话长了新 mention：before={before:?} after={after:?}"
    );
}
