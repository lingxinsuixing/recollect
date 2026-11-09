//! `recollect-core` agent 面子命令（export/search/show/notebook/edit）的黑盒判据：
//! 真进程 + 真数据目录，与 `admin_cli.rs`（注册面）互补。这一侧断 agent 自助读写的
//! 可见行为、导出文件头与 `core_access_log` 的 `source=cli` 审计行；
//! **任何输出都不许出现 key 内容**（本文件也从不读取 key 文件的内容）。

use rusqlite::Connection;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// `core_access_log` 一行的投影（clippy 的 `type_complexity` 只认有名字的元组）。
type LogRow = (String, String, Option<String>, String, Option<String>);

fn run(dir: &Path, args: &[&str]) -> Output {
    run_with(dir, args, None)
}

/// 带 stdin 跑一把：`edit --stdin` 之类从标准输入读 JSON 的子命令用。
/// 子进程可能不读完 stdin 就退出（越界段拒绝），写失败不作为判据。
fn run_with(dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_recollect-core"));
    cmd.arg("--data-dir").arg(dir).args(args);
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    // spawn() 不像 output() 那样自动接管 std 流：判据要读输出，这里显式接管。
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().expect("recollect-core binary");
    if let Some(text) = stdin {
        let _ = child.stdin.take().unwrap().write_all(text.as_bytes());
    }
    child.wait_with_output().expect("recollect-core run")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn edit(dir: &Path, agent: &str, section: &str, json: &str) -> Output {
    run_with(dir, &["edit", agent, section, "--stdin"], Some(json))
}

fn access_log(dir: &Path) -> Vec<LogRow> {
    let conn = Connection::open(dir.join("core.db")).unwrap();
    let mut st = conn
        .prepare(
            "SELECT caller_agent_id, tool, target_section, result, detail \
             FROM core_access_log ORDER BY id",
        )
        .unwrap();
    st.query_map([], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

/// 收集一个目录里的文件路径，按扩展名归类（导出文件判据用）。
fn files_by_ext(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| {
            let path = e.unwrap().path();
            let ext = path
                .extension()
                .map(|x| x.to_string_lossy().to_string())
                .unwrap_or_default();
            (ext, path)
        })
        .collect();
    out.sort();
    out
}

// ── ① export：md+json 两件、文件头三要素、--format 形态 ─────────────────────

#[test]
fn export_writes_md_and_json_with_a_plaintext_header() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(dir.path(), &["register", "agent-exp"]).status.code(), Some(0));
    let out = edit(
        dir.path(),
        "agent-exp",
        "affect",
        r#"{"affect":{"subject_kind":"user","label":"steady","confidence":0.7}}"#,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));

    let out = run(dir.path(), &["export", "agent-exp"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let export_root = dir.path().join("exports").join("agent-exp");
    let mut md = None;
    let mut json = None;
    for (ext, path) in files_by_ext(&export_root) {
        match ext.as_str() {
            "md" => md = Some(path),
            "json" => json = Some(path),
            other => panic!("unexpected export file .{other}: {}", path.display()),
        }
    }
    let md = md.expect("md 文件要落盘");
    let json = json.expect("json sidecar 要落盘");

    let md_text = std::fs::read_to_string(&md).unwrap();
    assert!(
        md_text.contains("core 私有记忆明文导出"),
        "文件头缺「明文导出」性质标注: {md_text}"
    );
    assert!(md_text.contains("UTC"), "文件头缺 UTC 时间标注: {md_text}");
    assert!(md_text.contains("agent-exp"), "文件头缺 agent_id: {md_text}");
    assert!(
        md_text.contains("steady"),
        "md 要按 brief 的自然语言形状带内容: {md_text}"
    );

    let payload: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&json).unwrap()).unwrap();
    assert_eq!(payload["agent_id"], "agent-exp", "sidecar 是结构化载荷");

    let echoed = text(&out);
    assert!(echoed.contains("exports"), "CLI 要报告导出落点: {echoed}");
    assert!(
        !echoed.contains(&hex_of_key(dir.path(), "agent-exp")),
        "输出绝不出现 key 内容"
    );

    // --format md 只落 md 一件；--out 换导出根目录。
    let out2 = dir.path().join("out2");
    let out = run(
        dir.path(),
        &[
            "export",
            "agent-exp",
            "--out",
            out2.to_str().unwrap(),
            "--format",
            "md",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let files = files_by_ext(&out2.join("agent-exp"));
    assert_eq!(files.len(), 1, "只要 md 一件: {files:?}");
    assert_eq!(files[0].0, "md");
}

fn hex_of_key(dir: &Path, agent: &str) -> String {
    let path = rc_core::key_path(dir, agent);
    if path.exists() {
        std::fs::read(&path)
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    } else {
        String::new()
    }
}

// ── ① 补：带日期戳的导出文件不进 git ────────────────────────────────────────

#[test]
fn exports_paths_are_git_ignored() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("rc-core 在仓库两层之下")
        .to_path_buf();
    assert!(root.join(".git").exists(), "本判据跑在 git checkout 里: {}", root.display());
    let out = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["check-ignore", "-v", "--", "exports/agent-x/core-20261107T153045Z.md"])
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "exports/ 路径没有被 ignore: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let matched = String::from_utf8_lossy(&out.stdout);
    assert!(matched.contains("exports"), "命中的规则应是 exports 规则: {matched}");
}

// ── ② search：命中带 section 与片段；空命中是合法读数 ───────────────────────

#[test]
fn search_hits_carry_section_and_snippet_and_empty_hits_are_legal() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(dir.path(), &["register", "agent-sea"]).status.code(), Some(0));
    let out = edit(
        dir.path(),
        "agent-sea",
        "relationship",
        r#"{"relationship":{"trust":0.7,"text":"zebra 协作顺利"}}"#,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));

    let out = run(dir.path(), &["search", "agent-sea", "zebra"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("relationship"), "命中行要带 section 名: {stdout}");
    assert!(stdout.contains("zebra"), "命中行要带上下文片段: {stdout}");

    // 空命中：合法空读数，不是故障（rc=0、无命中行）。
    let out = run(dir.path(), &["search", "agent-sea", "qqqq_no_such_word"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(out.stdout.is_empty(), "空命中不该有输出: {}", String::from_utf8_lossy(&out.stdout));
}

// ── ③ show：闭集越界报错带清单；指定段有内容才出 ───────────────────────────

#[test]
fn show_rejects_unknown_sections_with_the_closed_list() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(dir.path(), &["register", "agent-show"]).status.code(), Some(0));

    let out = run(dir.path(), &["show", "agent-show", "--section", "bogus"]);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    for name in ["l0", "persona", "affect", "chinese_expression", "notebook", "recent"] {
        assert!(stderr.contains(name), "越界报错要带闭集清单（缺 {name}）: {stderr}");
    }

    let out = run(dir.path(), &["show", "agent-show"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("[常驻简报]"),
        "全量 show 出简报形状: {}",
        text(&out)
    );

    let out = edit(
        dir.path(),
        "agent-show",
        "relationship",
        r#"{"relationship":{"trust":0.6,"text":"shark days"}}"#,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let out = run(dir.path(), &["show", "agent-show", "--section", "relationship"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("[关系与交互]"), "指定段出该段的渲染文本: {stdout}");
    assert!(stdout.contains("shark"), "段内容要在场: {stdout}");

    // 没内容的段 = 段缺席（沉默匹配视图内容）：rc=0、无输出。
    let out = run(dir.path(), &["show", "agent-show", "--section", "persona"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(out.stdout.is_empty(), "缺席段不该有输出: {}", String::from_utf8_lossy(&out.stdout));
}

// ── ④ notebook：append → search 往返；与文档检索不混尺 ─────────────────────

#[test]
fn notebook_append_then_search_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(dir.path(), &["register", "agent-note"]).status.code(), Some(0));

    // --help 记载正文的给法（工单：选一种并写进 --help）。
    let out = run(dir.path(), &["notebook", "agent-note", "append", "--help"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("--body"),
        "append --help 要写明正文走 --body: {}",
        text(&out)
    );

    let out = run(
        dir.path(),
        &[
            "notebook",
            "agent-note",
            "append",
            "--title",
            "斑马会议",
            "--tags",
            "work,deep",
            "--body",
            "讨论了 zebra 计划的下一刀",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("appended n-"),
        "append 回执给句柄: {}",
        text(&out)
    );

    let out = run(dir.path(), &["notebook", "agent-note", "search", "zebra"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("斑马会议"), "search 命中带标题: {stdout}");
    assert!(stdout.contains("n-"), "search 命中带句柄: {stdout}");

    let out = run(dir.path(), &["notebook", "agent-note", "list"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("斑马会议"),
        "list 出标题: {}",
        text(&out)
    );

    // 小本本不进文档检索：`core search` 对同一词是空命中。
    let out = run(dir.path(), &["search", "agent-note", "zebra"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(
        out.stdout.is_empty(),
        "文档检索查不到小本本正文: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

// ── ⑤ edit：三段走通 + access_log 落 source=cli；persona 拒并指路 ──────────

#[test]
fn edit_writes_three_sections_with_cli_source_log_and_refuses_persona() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(run(dir.path(), &["register", "agent-edit"]).status.code(), Some(0));

    let out = edit(
        dir.path(),
        "agent-edit",
        "affect",
        r#"{"affect":{"subject_kind":"user","label":"calm","confidence":0.8}}"#,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let out = edit(
        dir.path(),
        "agent-edit",
        "relationship",
        r#"{"relationship":{"trust":0.6,"text":"互信"}}"#,
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let out = edit(dir.path(), "agent-edit", "chinese_expression", r#"{"enabled":true}"#);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));

    // persona 拒绝并指向提案流。
    let out = edit(
        dir.path(),
        "agent-edit",
        "persona",
        r#"{"patch":{"core_text":"self rewrite"},"reason":"想改"}"#,
    );
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("proposal flow"), "拒辞要指向提案流: {stderr}");
    assert!(stderr.contains("core_persona_propose"), "拒辞要点名正确入口: {stderr}");

    // 其余段同样拒绝并给闭集/入口。
    let out = edit(dir.path(), "agent-edit", "loops", r#"{"item":{}}"#);
    assert_eq!(out.status.code(), Some(3), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("chinese_expression"),
        "闭集清单要在场: {}",
        text(&out)
    );

    // access_log：三段各落一行 source=cli 的 ok；persona/loops 的拒绝落 denied。
    let rows = access_log(dir.path());
    for (caller, _, _, _, detail) in &rows {
        assert_eq!(caller, "agent-edit", "caller 是 agent 本体");
        assert!(
            detail.as_deref().unwrap_or_default().contains("source=cli"),
            "detail 要带 source=cli: {detail:?}"
        );
    }
    let ok_rows: Vec<&LogRow> =
        rows.iter().filter(|(_, _, _, result, _)| result == "ok").collect();
    assert_eq!(ok_rows.len(), 3, "三段各一行 ok: {rows:?}");
    let mut ok_tools: Vec<&str> = ok_rows.iter().map(|(_, tool, _, _, _)| tool.as_str()).collect();
    ok_tools.sort_unstable();
    assert_eq!(
        ok_tools,
        ["core_affect_update", "core_chinese_expression_set", "core_relationship_update"],
        "三段各对应同名 MCP 写工具: {ok_tools:?}"
    );
    for (_, _, section, _, _) in &ok_rows {
        assert!(section.is_some(), "ok 行要带 target_section");
    }
    let denied_rows: Vec<&LogRow> =
        rows.iter().filter(|(_, _, _, result, _)| result == "denied").collect();
    assert_eq!(denied_rows.len(), 2, "两次拒绝各一行: {rows:?}");
    assert!(
        denied_rows.iter().all(|(_, tool, _, _, _)| tool == "cli_edit"),
        "拒绝行记在 cli_edit 名下"
    );
    let persona_row = rows
        .iter()
        .find(|(_, _, section, _, _)| section.as_deref() == Some("persona"))
        .expect("persona 拒绝要有审计行");
    assert_eq!(persona_row.3, "denied", "persona 行是 denied: {persona_row:?}");

    // 写真的落了：读回（同 CLI 的读面）。
    let out = run(dir.path(), &["search", "agent-edit", "calm"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("affect") && stdout.contains("calm"), "affect 写入可读回: {stdout}");
    let out = run(dir.path(), &["show", "agent-edit", "--section", "chinese_expression"]);
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("[中文表达优化]"),
        "开关写入可读回: {}",
        text(&out)
    );
}
