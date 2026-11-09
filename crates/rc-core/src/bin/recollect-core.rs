//! core CLI：操作员注册面 + agent 自助面。
//!
//! 注册面（register/list/revoke）在 MCP 之外注册 / 列出 / 撤销 agent：agent 不能给自己
//! 注册，所以注册与撤销只从这台入口发生。agent 面（export/search/show/notebook/edit）
//! 是 2026-11-07 改判后的 agent 专用口：①core.db 零备份是真缺口，导出备份为文档、agent
//! 自救；②不依赖 MCP 通道的访问（换装断连窗口期照常读写）；③轻操作一行命令。形状走
//! 子命令（不走短旗标，agent 打命令时子命令不易歧义）。
//!
//! agent 面的读走 [`rc_core::CoreStore::load_doc`]/[`rc_core::CoreStore::notebook_index`]
//! 加 render 的单一渲染器（不造第二把渲染尺）；写复用 ops 层校验与 `core_access_log`
//! （detail 恒带 `source=cli`，tool 名与 MCP 写工具一一对应），不造第二把校验尺。
//!
//! 纪律：**从不打印 key 内容**。退出码：0 成功；3 操作失败（非法 agent_id、IO/SQL 错、
//! 未注册、section 越界、校验拒绝）。

use clap::{Parser, Subcommand, ValueEnum};
use rc_core::{
    key_path, Affect, CorePayload, CoreStore, Relationship, DEFAULT_INCLUDE, SEC_AFFECT,
    SEC_CHINESE_EXPRESSION, SEC_RELATIONSHIP, doc_search, render_brief_with_notebook,
};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

/// agent 面写路径的 device_id：CLI 写入如实记为 `cli`。
const DEVICE: &str = "cli";

#[derive(Parser)]
#[command(
    name = "recollect-core",
    version,
    about = "core.db faces: operator registry (register/list/revoke) + agent self-service \
             (export/search/show/notebook/edit), outside MCP"
)]
struct Args {
    /// 数据目录：`core.db` 与 `keys/` 都在这里。
    #[arg(long, default_value = "./data", value_name = "DIR")]
    data_dir: PathBuf,
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 注册 agent；key 文件不存在时生成，已存在则沿用。
    Register { agent_id: String },
    /// 按 `agent_id` 排序列出注册表，每行 `agent_id\tkey_id\tdoc_version\tupdated_at_ms`。
    List,
    /// 撤销 agent：删注册行与 `core_doc_<h>`/`core_rev_<h>`/`core_note_<h>` 三张表。
    Revoke {
        agent_id: String,
        /// 同时删除 `<data-dir>/keys/core_<h>.key`（默认保留）。
        #[arg(long)]
        delete_key: bool,
    },
    /// 导出本 agent 的核心记忆：**明文备份文档**（md 为主 + json sidecar，缺省 both）。
    Export {
        agent_id: String,
        /// 导出根目录，文件落 `<DIR>/<agent>/core-<UTC日期戳>.{md,json}`；
        /// 缺省 `<data-dir>/exports`。
        #[arg(long, value_name = "DIR")]
        out: Option<PathBuf>,
        /// 导出形态：md（brief 的自然语言形状）、json（结构化载荷 sidecar）或 both（缺省）。
        #[arg(long, value_enum, default_value_t = ExportFormat::Both)]
        format: ExportFormat,
    },
    /// 跨 section 子串检索解密后的文档（多词「或」，命中行带区名与片段）；
    /// 小本本另有 `core notebook search`，两把尺不混。
    Search {
        agent_id: String,
        /// 检索词（空格分隔，命中任一即算）。
        query: Vec<String>,
    },
    /// 打印常驻简报形状的各 section（或 `--section` 指定段）的渲染文本；
    /// 缺席的段不打印（沉默匹配视图内容）。
    Show {
        agent_id: String,
        /// 只打印这一段；段名闭集见越界报错清单。
        #[arg(long, value_name = "NAME")]
        section: Option<String>,
    },
    /// 小本本三件事：append / list / search（语义与 MCP 的 core_notebook_* 一致）。
    Notebook {
        agent_id: String,
        #[command(subcommand)]
        action: NotebookAction,
    },
    /// 直改三个与 MCP 写工具一一对应的段：affect / relationship / chinese_expression。
    /// 同校验、同原子路径、同 access_log；其余段明确拒绝并指向正确入口。
    Edit {
        agent_id: String,
        /// 要改的段：affect | relationship | chinese_expression。
        section: String,
        /// 从标准输入读 JSON（必需），形状与 MCP 工具参数一致：
        /// `{"affect":{...}}` / `{"relationship":{...}}` / `{"enabled":true|false}`。
        #[arg(long, required = true)]
        stdin: bool,
    },
}

/// `core export --format` 的闭集。
#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum ExportFormat {
    Md,
    Json,
    Both,
}

#[derive(Subcommand)]
enum NotebookAction {
    /// 追加一条。正文用 `--body` 给出（不设 --stdin：正文 ≤2000 字符，一行命令给全）。
    Append {
        /// 标题（可省）。
        #[arg(long)]
        title: Option<String>,
        /// 逗号分隔的标签（≤8 条，每条 ≤32 字符）。
        #[arg(long, value_name = "TAGS")]
        tags: Option<String>,
        /// 正文（≤2000 字符）。
        #[arg(long)]
        body: String,
    },
    /// 列出（新的在前）。`--tag` 单标签过滤；`--limit` 缺省 50、封顶 200。
    List {
        #[arg(long)]
        tag: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// 子串检索 title/tags/body（多词「或」；`--limit` 缺省 20、封顶 200）。
    Search {
        /// 检索词（空格分隔，命中任一即算）。
        query: Vec<String>,
        #[arg(long)]
        limit: Option<usize>,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("error: {msg}");
            ExitCode::from(3)
        }
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn run(args: Args) -> Result<(), String> {
    let store = CoreStore::open(&args.data_dir).map_err(|e| e.to_string())?;
    match args.command {
        Cmd::Register { agent_id } => {
            store.register_agent(&agent_id, now_ms()).map_err(|e| e.to_string())?;
            println!("registered {agent_id}");
            Ok(())
        }
        Cmd::List => {
            for row in store.list_agents().map_err(|e| e.to_string())? {
                println!(
                    "{}\t{}\t{}\t{}",
                    row.agent_id, row.key_id, row.doc_version, row.updated_at_ms
                );
            }
            Ok(())
        }
        Cmd::Revoke { agent_id, delete_key } => {
            // 先撤销注册：非法 id 与未注册都在这一步拒掉，key 文件不会被动到。
            store.revoke_agent(&agent_id).map_err(|e| e.to_string())?;
            if delete_key {
                delete_key_file(&args.data_dir, &agent_id)?;
            }
            println!("revoked {agent_id}");
            Ok(())
        }
        Cmd::Export { agent_id, out, format } => {
            cmd_export(&store, &args.data_dir, &agent_id, out, format)
        }
        Cmd::Search { agent_id, query } => cmd_search(&store, &agent_id, &query),
        Cmd::Show { agent_id, section } => cmd_show(&store, &agent_id, section.as_deref()),
        Cmd::Notebook { agent_id, action } => cmd_notebook(&store, &agent_id, action),
        Cmd::Edit { agent_id, section, stdin: _ } => cmd_edit(&store, &agent_id, &section),
    }
}

/// 删除 key 文件；已经不存在也算成功（撤销可以重试）。
fn delete_key_file(data_dir: &Path, agent_id: &str) -> Result<(), String> {
    match std::fs::remove_file(key_path(data_dir, agent_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("cannot delete key file: {e}")),
    }
}

// ── agent 面：export ─────────────────────────────────────────────────────────

fn cmd_export(
    store: &CoreStore,
    data_dir: &Path,
    agent_id: &str,
    out: Option<PathBuf>,
    format: ExportFormat,
) -> Result<(), String> {
    let now = now_ms();
    let payload = load_payload(store, agent_id)?;
    let notebook = store.notebook_index(agent_id).map_err(|e| e.to_string())?;
    let version = store.doc_version(agent_id).map_err(|e| e.to_string())?;
    let brief = render_brief_with_notebook(
        &payload,
        &primary_user(&payload),
        "cli",
        0,
        &DEFAULT_INCLUDE,
        &notebook,
        None,
        now,
    );
    let (iso, stamp) = utc_parts(now);
    let dir = out.unwrap_or_else(|| data_dir.join("exports")).join(agent_id);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let md_path = dir.join(format!("core-{stamp}.md"));
    let json_path = dir.join(format!("core-{stamp}.json"));
    let want_md = matches!(format, ExportFormat::Md | ExportFormat::Both);
    let want_json = matches!(format, ExportFormat::Json | ExportFormat::Both);

    if want_md {
        // 文件头三要素：明文导出性质 + UTC 时间 + agent_id。导出文件不进 git
        // （`.gitignore` 的 `**/exports/` 规则）。
        let mut md = String::new();
        md.push_str("# core 私有记忆明文导出\n\n");
        md.push_str(&format!("- agent_id: {agent_id}\n"));
        md.push_str(&format!("- 导出时间（UTC）: {iso}\n"));
        md.push_str(&format!("- doc_version: {version}\n"));
        md.push_str(
            "- 性质：这是 core.db 加密载荷的**明文导出**（人格、关系、情绪等私密内容以明文在场）；\
             只落本机 exports 目录，不进 git、不进召回、不进任何同步面。\n\n---\n\n",
        );
        md.push_str(&brief.text);
        md.push('\n');
        std::fs::write(&md_path, md).map_err(|e| e.to_string())?;
    }
    if want_json {
        let json = serde_json::to_vec_pretty(&payload).map_err(|e| e.to_string())?;
        std::fs::write(&json_path, json).map_err(|e| e.to_string())?;
    }
    if want_md {
        println!("{}", md_path.display());
    }
    if want_json {
        println!("{}", json_path.display());
    }
    Ok(())
}

/// UTC 时刻两件套：文件头的人读 ISO 行与文件名的日期戳。日历算术复用
/// rc-contract 的 civil 渲染（`tz_offset_min=0` 即 UTC），秒数从纪元秒直接取余——
/// 不引入 chrono，也不写第二套日历算法。
fn utc_parts(now_ms: i64) -> (String, String) {
    let ss = now_ms.div_euclid(1000).rem_euclid(60);
    let hm = rc_contract::format_local_ms(now_ms, 0); // `YYYY-MM-DD HH:MM`
    let date = rc_contract::format_local_date(now_ms, 0); // `YYYY-MM-DD`
    let iso = format!("{}:{ss:02}Z", hm.replacen(" ", "T", 1));
    let digits = |s: &str| s.chars().filter(|c| c.is_ascii_digit()).collect::<String>();
    let stamp = format!("{}T{}{ss:02}Z", digits(&date), &digits(&hm)[8..]);
    (iso, stamp)
}

fn primary_user(payload: &CorePayload) -> String {
    payload
        .subjects
        .iter()
        .find(|s| s.is_primary)
        .map(|s| s.subject_id.clone())
        .unwrap_or_default()
}

/// 读当前文档：已注册但还没写过 ⇒ 空载荷（合法空读数）；未注册 ⇒ 报错。
fn load_payload(store: &CoreStore, agent_id: &str) -> Result<CorePayload, String> {
    Ok(store
        .load_doc(agent_id)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| CorePayload::empty(agent_id)))
}

// ── agent 面：search ─────────────────────────────────────────────────────────

fn cmd_search(store: &CoreStore, agent_id: &str, query: &[String]) -> Result<(), String> {
    let query = query.join(" ");
    let payload = load_payload(store, agent_id)?;
    for hit in doc_search(&payload, &query).map_err(|e| e.to_string())? {
        println!("{}\t{}", hit.section, hit.snippet);
    }
    Ok(())
}

// ── agent 面：show ───────────────────────────────────────────────────────────

/// brief 渲染顺序的段名闭集（render.rs 的既有常量；`handoff_prev` 是 hook 面的段，
/// 从不出现在 core.db 的简报里，不在闭集内）。
const SECTION_ORDER: [&str; 11] = [
    rc_core::SEC_L0,
    rc_core::SEC_CHINESE_EXPRESSION,
    rc_core::SEC_PERSONA,
    rc_core::SEC_SUBJECT,
    rc_core::SEC_RELATIONSHIP,
    rc_core::SEC_AFFECT,
    rc_core::SEC_LOOPS,
    rc_core::SEC_HANDOFF,
    rc_core::SEC_NOTEBOOK,
    rc_core::SEC_PINNED,
    rc_core::SEC_RECENT,
];

fn cmd_show(store: &CoreStore, agent_id: &str, section: Option<&str>) -> Result<(), String> {
    // 闭集校验先行：越界名在碰库之前就拿到清单报错。
    if let Some(name) = section {
        if !SECTION_ORDER.contains(&name) {
            return Err(format!(
                "unknown section {name:?}; sections of the resident brief: {}",
                SECTION_ORDER.join(", ")
            ));
        }
    }
    let (text, present) = rendered_brief(store, agent_id)?;
    match section {
        None => println!("{text}"),
        Some(name) => {
            // 段缺席 = 合法空读数（沉默匹配视图内容）。
            if !present.iter().any(|key| key == &name) {
                return Ok(());
            }
            let blocks = split_blocks(&text);
            if blocks.len() != present.len() + 1 {
                return Err("brief sections and text blocks disagree".to_string());
            }
            let index = present.iter().position(|key| *key == name).expect("checked above");
            // blocks[0] 是头部；其后每块按渲染顺序对应 present 里的一个段。
            println!("{}", blocks[index + 1].trim_end());
        }
    }
    Ok(())
}

/// 用 render 的单一渲染器出整篇简报（纯函数，不写注入日志），并把实际在场的段
/// 按渲染顺序列出来（`Brief::section_tokens` 只含渲染出来的区）。
fn rendered_brief(store: &CoreStore, agent_id: &str) -> Result<(String, Vec<&'static str>), String> {
    let payload = load_payload(store, agent_id)?;
    let notebook = store.notebook_index(agent_id).map_err(|e| e.to_string())?;
    let brief = render_brief_with_notebook(
        &payload,
        &primary_user(&payload),
        "cli",
        0,
        &DEFAULT_INCLUDE,
        &notebook,
        None,
        now_ms(),
    );
    let present: Vec<&'static str> = SECTION_ORDER
        .iter()
        .copied()
        .filter(|key| brief.section_tokens.keys().any(|k| k == key))
        .collect();
    Ok((brief.text, present))
}

/// 把渲染文本切成块：每个以 `[` 开头的行开启一块（第一块是头部）。
/// 区内容行都以 `- ` 前缀渲染，不会撞 `[`。
fn split_blocks(text: &str) -> Vec<&str> {
    let mut blocks: Vec<&str> = Vec::new();
    let mut current = 0usize;
    let mut offset = 0usize;
    let mut seen_any = false;
    for line in text.lines() {
        if seen_any && line.starts_with('[') {
            blocks.push(&text[current..offset]);
            current = offset;
        }
        seen_any = true;
        offset += line.len() + 1; // '\n'
    }
    blocks.push(&text[current..]);
    blocks
}

// ── agent 面：notebook ───────────────────────────────────────────────────────

fn cmd_notebook(store: &CoreStore, agent_id: &str, action: NotebookAction) -> Result<(), String> {
    match action {
        NotebookAction::Append { title, tags, body } => {
            let tags: Vec<String> = tags
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|tag| !tag.is_empty())
                .map(str::to_string)
                .collect();
            let now = now_ms();
            match store.notebook_append(agent_id, title.as_deref(), &tags, &body, None, DEVICE, now)
            {
                Ok(appended) => {
                    log_cli(store, agent_id, "core_notebook_append", None, "ok", None, now);
                    println!("appended {}", appended.id);
                }
                Err(e) => {
                    log_cli(store, agent_id, "core_notebook_append", None, "denied", Some(&code_of(&e)), now);
                    return Err(e.to_string());
                }
            }
            Ok(())
        }
        NotebookAction::List { tag, limit } => {
            let list = store
                .notebook_list(agent_id, limit, tag.as_deref())
                .map_err(|e| e.to_string())?;
            print_notebook(&list);
            Ok(())
        }
        NotebookAction::Search { query, limit } => {
            let query = query.join(" ");
            let list = store.notebook_search(agent_id, &query, limit).map_err(|e| e.to_string())?;
            print_notebook(&list);
            Ok(())
        }
    }
}

/// list/search 共用的行形：`id\ttitle\ttags\tcreated_at_ms`（零正文字段，照 ops 层投影）。
/// 末行报 `unlisted`（过滤后在场但没列出的条数，ops 层的既有读数）。
fn print_notebook(list: &rc_core::NotebookList) {
    for note in &list.notes {
        println!("{}\t{}\t{}\t{}", note.id, note.title, note.tags.join(","), note.created_at_ms);
    }
    println!("unlisted {}", list.unlisted);
}

// ── agent 面：edit ───────────────────────────────────────────────────────────

/// `core edit` 的三个可编辑段，与 MCP 写工具一一对应。
#[derive(Copy, Clone)]
enum EditTarget {
    Affect,
    Relationship,
    ChineseExpression,
}

impl EditTarget {
    /// access_log 的 tool 名：与 MCP 写工具同名（不造第二把尺）。
    fn tool(self) -> &'static str {
        match self {
            EditTarget::Affect => "core_affect_update",
            EditTarget::Relationship => "core_relationship_update",
            EditTarget::ChineseExpression => "core_chinese_expression_set",
        }
    }
}

/// 写失败的审计码 + 面向人的错误文本。
type EditOutcome = Result<u32, (String, String)>;

fn cmd_edit(store: &CoreStore, agent_id: &str, section: &str) -> Result<(), String> {
    let now = now_ms();
    // 段的闭集先于一切：拒掉的段记一条 denied（审计有痕），并指向正确入口。
    let target = match section {
        SEC_AFFECT => EditTarget::Affect,
        SEC_RELATIONSHIP => EditTarget::Relationship,
        SEC_CHINESE_EXPRESSION => EditTarget::ChineseExpression,
        other => {
            let _ = store.log_access(
                agent_id,
                "cli_edit",
                Some(other),
                "denied",
                Some("source=cli; section_not_editable"),
                now,
            );
            return Err(edit_pointer(other));
        }
    };
    let mut input = String::new();
    let value = std::io::stdin()
        .read_to_string(&mut input)
        .ok()
        .and_then(|_| serde_json::from_str::<serde_json::Value>(input.trim()).ok());
    let Some(value) = value else {
        log_cli(store, agent_id, target.tool(), Some(section), "denied", Some("bad_json"), now);
        return Err("stdin is not valid JSON; the shape is the MCP tool's argument object".to_string());
    };

    let write: EditOutcome = match target {
        EditTarget::Affect => {
            let field = value.get("affect").cloned().unwrap_or(serde_json::Value::Null);
            match serde_json::from_value::<Affect>(field) {
                Ok(affect) => store
                    .affect_update(agent_id, affect, DEVICE, now)
                    .map_err(store_failure),
                Err(_) => Err((
                    "bad_affect".to_string(),
                    "stdin JSON must carry an \"affect\" object with the \
                     core_affect_update argument shape"
                        .to_string(),
                )),
            }
        }
        EditTarget::Relationship => {
            let field = value.get("relationship").cloned().unwrap_or(serde_json::Value::Null);
            match serde_json::from_value::<Relationship>(field) {
                Ok(relationship) => store
                    .relationship_update(agent_id, relationship, DEVICE, now)
                    .map_err(store_failure),
                Err(_) => Err((
                    "bad_relationship".to_string(),
                    "stdin JSON must carry a \"relationship\" object with the \
                     core_relationship_update argument shape"
                        .to_string(),
                )),
            }
        }
        EditTarget::ChineseExpression => {
            match value.get("enabled").and_then(serde_json::Value::as_bool) {
                Some(enabled) => store
                    .chinese_expression_set(agent_id, enabled, DEVICE, now)
                    .map_err(store_failure),
                None => Err((
                    "bad_enabled".to_string(),
                    "stdin JSON must be {\"enabled\": true|false} (the \
                     core_chinese_expression_set argument shape)"
                        .to_string(),
                )),
            }
        }
    };
    match write {
        Ok(version) => {
            log_cli(store, agent_id, target.tool(), Some(section), "ok", None, now);
            println!("ok {section} v{version}");
            Ok(())
        }
        Err((code, msg)) => {
            log_cli(store, agent_id, target.tool(), Some(section), "denied", Some(&code), now);
            Err(msg)
        }
    }
}

/// 拒辞：指向该段的正确入口（persona 走提案流；handoff 走交接工具；notebook 走
/// 专用子命令；l0 不可自改；其余段给可编辑闭集）。
fn edit_pointer(section: &str) -> String {
    match section {
        "persona" => {
            "persona is not editable via `core edit`: use the proposal flow \
             (MCP core_persona_propose to submit, core_persona_apply/reject to review)"
                .to_string()
        }
        "handoff" => {
            "handoff is not a `core edit` section: use the MCP tools \
             core_handoff_write / core_handoff_close"
                .to_string()
        }
        "notebook" => "notebook is not a doc section: use `core notebook <agent_id> append`".to_string(),
        "l0" => "l0 cannot be self-edited: only the user may change it at the document level".to_string(),
        other => format!(
            "section {other:?} is not editable via `core edit`; the editable sections are \
             affect, relationship, chinese_expression (persona: proposal flow; handoff: \
             core_handoff_*; notebook: `core notebook <agent_id> append`)"
        ),
    }
}

fn store_failure(e: rc_core::CoreError) -> (String, String) {
    (code_of(&e), e.to_string())
}

/// store 错误的审计码（照 mcp 层按码分流的口径：找不到与拒绝分得开）。
fn code_of(e: &rc_core::CoreError) -> String {
    match e {
        rc_core::CoreError::Unregistered => "unregistered",
        rc_core::CoreError::NotFound => "not_found",
        rc_core::CoreError::BadAgentId => "bad_agent_id",
        rc_core::CoreError::BadPayload(_) => "bad_payload",
        _ => "store_error",
    }
    .to_string()
}

/// CLI 写路径的 `core_access_log` 行：detail 恒带 `source=cli`（失败再补错误码）。
/// 写日志失败不影响回执（账是账，答案是答案，与 mcp 层同一姿态）。
fn log_cli(
    store: &CoreStore,
    agent_id: &str,
    tool: &str,
    section: Option<&str>,
    result: &str,
    code: Option<&str>,
    now: i64,
) {
    let detail = match code {
        Some(code) => format!("source=cli; {code}"),
        None => "source=cli".to_string(),
    };
    let _ = store.log_access(agent_id, tool, section, result, Some(&detail), now);
}
