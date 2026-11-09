//! hook 集成测试的**共享夹具**（P-65 hook 半边，任务 11 落地）。原本住在
//! `probe_cli.rs` 第 17–83 行的 `NOW`/`TOK`/`bin`/`hook`/`hook_stdin`/`hook_stdin_bytes`/`seed`/
//! `data_of`/`now_arg`/`past_arg` 原样搬到这里，每一项加 `pub`；`probe_cli.rs` 改走
//! `mod common; use common::*;`，17 条用例逐字不动。
//!
//! P3 又补了一份 `seed_person`：目录要实体 + 共享出处的 card，mcp 那一侧的同名夹具不共享
//! （Cargo 测试隔离 = `tests/<crate>/` 之间没有跨 crate 的家）⇒ `seed_person` 漂了不会让两面
//! 互相同意，只会让各自那一面对自己的期望红。
//!
//! `tests/<目录>/mod.rs` 不是独立 target ⇒ 本次迁移的**唯一收据**是 hook 的 24 条读数一格不动
//! （既有用例在本模块残留的允许列表见文件级 `#![allow(dead_code)]`）。
#![allow(dead_code)]
use rc_memory::{
    put_card, put_entity, AnchorRole, EntityContent, FactContent, Provenance, Scope, ScopeKind,
};
use rc_store::AgentRoot;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub const NOW: i64 = 1_700_000_000_000;
pub const TOK: &str = "提示面合成词";

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_recollect-hook")
}

pub fn hook(args: &[&str]) -> std::process::Output {
    Command::new(bin()).args(args).output().expect("hook binary")
}

/// 偏离 brief 的第二处（同样是夹具管线而非断言，逐字点名在 `task-9b1-report.md` §偏离 D1）：
/// Step 4 的 `hook_stdin` helper 只 `Stdio::piped()` 了 **stdin**，于是 `wait_with_output()` 交回的
/// `stdout`/`stderr` 结构性地是空的（子进程的输出直接进了测试运行器的终端——实测那条
/// `[recollect] card:…` 就打印在测试名后面）。而 `宿主stdin的JSON只取prompt字段` 的断言
/// （同 Step 4）读的正是 `out.stdout.lines().count()` ⇒ 不改实现，任何生产码都过不了这一格，
/// 这一条用例就变成"一条从没红过的守卫"（本档铁律点名的形状），Step 8 证伪第 3 条也失去读数。
/// 补的是**采集**（让断言可观测，方向严格更严：现在它能红了），十条断言逐字未动。
pub fn hook_stdin(args: &[&str], blob: &str) -> std::process::Output {
    hook_stdin_bytes(args, blob.as_bytes())
}

/// R77 的那一半夹具：`hook_stdin` 只能递能塞进 `&str` 的字节，而这条判据要的正是
/// "**解不开的字节**"——所以把采集逻辑抽出来收 `&[u8]`，三支 `Stdio::piped()` 逐字照抄，
/// `hook_stdin` 改成它的薄封装（现有十条用例的 stdin 形状与断言一个字都没动）。
pub fn hook_stdin_bytes(args: &[&str], blob: &[u8]) -> std::process::Output {
    let mut child = Command::new(bin())
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("hook binary");
    child.stdin.take().unwrap().write_all(blob).unwrap();
    child.wait_with_output().unwrap()
}

/// 夹具：一条人工锚的 card + 建好的索引。**一律合成词**，正文只用于形状断言。
pub fn seed(data: &Path) -> rc_memory::keys::MemKey {
    let root = AgentRoot::open(data).unwrap();
    let m = root.memory();
    let k = put_card(
        m,
        &FactContent {
            kind: "decision".into(),
            title: format!("{TOK} 的口径"),
            body: "带这个词的行会被提示面召回。".into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    k
}

pub fn data_of(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join("data")
}
pub fn now_arg() -> String {
    format!("--now-ms={NOW}")
}
pub fn past_arg() -> String {
    format!("--now-ms={}", NOW + rc_memory::COOLDOWN_MS + 1)
}

/// P3 目录用的播种：一条实体（标题=别名之一）+ 一条**与它共享出处轮**的 card。
/// 返回 `()`（`p3_directory.rs` 两条用例要的是"目录里有东西"，不是哪个 id）——夹具返回
/// 的字符串只有 `p3_directory` 一个读者，类型层不给它开口。
pub fn seed_person(data: &Path, now_ms: i64) {
    let root = AgentRoot::open(data).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() };
    // 实体与 card **共享同一条出处轮**：目录的 `refs` 连的是出处而不是归属（P-56），
    // 这一串就是那条边的两端。
    let prov = vec![Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }];
    put_entity(
        m,
        &EntityContent {
            kind: "person".into(),
            title: "老王的口径".into(),
            scope: scope.clone(),
            aliases: vec!["某老板".into()],
        },
        &prov,
        now_ms,
    )
    .unwrap();
    put_card(
        m,
        &FactContent {
            kind: "decision".into(),
            title: "目录面的夹具 card".into(),
            body: "目录面只断形状。".into(),
            scope,
            valid_to_ms: None,
        },
        &prov,
        now_ms,
    )
    .unwrap();
    rc_index::rebuild(m, now_ms).unwrap();
}