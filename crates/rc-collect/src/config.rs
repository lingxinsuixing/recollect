//! `collect.toml` 的读取与校验。形状照 `crates/hub/src/hub_cfg.rs`：
//! 缺文件是 `Err`（采集任务没有配置必须响亮），配坏了也是 `Err`。
//!
//! 错误串只说字段名和规则，**不回声值**——配置里可能有路径与账号。
//! `converter` 是 argv 数组、直接 exec；改这份配置等价于获得本机执行权。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub limits: Limits,
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limits {
    pub quiet_ms: i64,
    pub max_files_per_run: u32,
    pub max_turns_per_day: u32,
    /// 24 小时回填预算（滚动窗口）：历史回填轮共用这一条额度。
    pub max_backfill_turns_per_day: u32,
}

/// `[limits] max_backfill_turns_per_day` 的缺省值。
pub const DEFAULT_MAX_BACKFILL_TURNS_PER_DAY: u32 = 20_000;

impl Default for Limits {
    fn default() -> Self {
        Limits {
            quiet_ms: 600_000,
            max_files_per_run: 3,
            max_turns_per_day: 300,
            max_backfill_turns_per_day: DEFAULT_MAX_BACKFILL_TURNS_PER_DAY,
        }
    }
}

/// 源的三种形态。
///
/// - `Files`：逐行追加的文本会话（Qoder）。水位是**字节偏移**。
/// - `Sqlite`：本机会话库（mcode / Hermes）。水位是**时间戳**。
/// - `Whole`：整份文件一个会话的压缩库（DSH 的 `session.v4.jsonl.zstd`）。
///   水位是 **{mtime_ms, size, head_sha256}**，offset 恒为 0——压缩流没有可切分的字节边界，
///   文件变了就整份重转。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Files,
    Sqlite,
    Whole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    pub kind: SourceKind,
    pub root: Option<PathBuf>,
    pub glob: Option<String>,
    pub db: Option<PathBuf>,
    pub cursor_sql: Option<String>,
    pub converter: Vec<String>,
    pub device_id: String,
    pub source_agent: String,
    pub project: String,
}

const LIMIT_KEYS: &[&str] = &[
    "quiet_ms",
    "max_files_per_run",
    "max_turns_per_day",
    "max_backfill_turns_per_day",
];
const SOURCE_KEYS: &[&str] = &[
    "name",
    "kind",
    "root",
    "glob",
    "db",
    "cursor_sql",
    "converter",
    "device_id",
    "source_agent",
    "project",
];

impl Config {
    /// 读并校验 `collect.toml`。缺文件、坏 TOML、坏字段一律 `Err`。
    pub fn load(path: &Path) -> Result<Config, String> {
        let text =
            std::fs::read_to_string(path).map_err(|_| "collect.toml is unreadable".to_string())?;
        let doc: toml::Table =
            toml::from_str(&text).map_err(|_| "collect.toml is not valid TOML".to_string())?;
        for key in doc.keys() {
            if key != "limits" && key != "source" {
                return Err(format!("collect.toml has unknown key `{key}`"));
            }
        }

        let limits = match doc.get("limits") {
            None => Limits::default(),
            Some(v) => parse_limits(v.as_table().ok_or("limits must be a table")?)?,
        };

        let arr = doc
            .get("source")
            .and_then(toml::Value::as_array)
            .ok_or("collect.toml has no [[source]]")?;
        if arr.is_empty() {
            return Err("collect.toml has no [[source]]".to_string());
        }

        let mut sources = Vec::with_capacity(arr.len());
        for (i, v) in arr.iter().enumerate() {
            let table = v
                .as_table()
                .ok_or_else(|| format!("source[{i}] must be a table"))?;
            sources.push(parse_source(i, table)?);
        }

        // name 与 (device_id, source_agent) 都必须在整份配置里唯一：
        // 同一身份挂两个源会把水位切成两半，谁也推不满。
        let mut names = BTreeSet::new();
        let mut ids = BTreeSet::new();
        for s in &sources {
            if !names.insert(s.name.clone()) {
                return Err(format!("source name `{}` is not unique", s.name));
            }
            if !ids.insert((s.device_id.clone(), s.source_agent.clone())) {
                return Err(format!(
                    "source `{}`: (device_id, source_agent) is not unique",
                    s.name
                ));
            }
        }

        Ok(Config { limits, sources })
    }
}

fn parse_limits(t: &toml::Table) -> Result<Limits, String> {
    for key in t.keys() {
        if !LIMIT_KEYS.contains(&key.as_str()) {
            return Err(format!("limits has unknown key `{key}`"));
        }
    }
    let mut limits = Limits::default();
    if let Some(v) = t.get("quiet_ms") {
        limits.quiet_ms = v.as_integer().ok_or("limits.quiet_ms must be an integer")?;
    }
    if let Some(v) = t.get("max_files_per_run") {
        let n = v
            .as_integer()
            .ok_or("limits.max_files_per_run must be an integer")?;
        limits.max_files_per_run =
            u32::try_from(n).map_err(|_| "limits.max_files_per_run must be a positive integer")?;
    }
    if let Some(v) = t.get("max_turns_per_day") {
        let n = v
            .as_integer()
            .ok_or("limits.max_turns_per_day must be an integer")?;
        limits.max_turns_per_day =
            u32::try_from(n).map_err(|_| "limits.max_turns_per_day must be a positive integer")?;
    }
    if let Some(v) = t.get("max_backfill_turns_per_day") {
        let n = v
            .as_integer()
            .ok_or("limits.max_backfill_turns_per_day must be an integer")?;
        limits.max_backfill_turns_per_day = u32::try_from(n)
            .map_err(|_| "limits.max_backfill_turns_per_day must be a positive integer")?;
    }
    if limits.quiet_ms <= 0 {
        return Err("limits.quiet_ms must be positive".to_string());
    }
    if limits.max_files_per_run == 0 {
        return Err("limits.max_files_per_run must be positive".to_string());
    }
    if limits.max_turns_per_day == 0 {
        return Err("limits.max_turns_per_day must be positive".to_string());
    }
    if limits.max_backfill_turns_per_day == 0 {
        return Err("limits.max_backfill_turns_per_day must be positive".to_string());
    }
    Ok(limits)
}

fn parse_source(i: usize, t: &toml::Table) -> Result<Source, String> {
    for key in t.keys() {
        if !SOURCE_KEYS.contains(&key.as_str()) {
            return Err(format!("source[{i}] has unknown key `{key}`"));
        }
    }
    let name = t
        .get("name")
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("source[{i}].name is required"))?
        .trim()
        .to_string();
    if name.is_empty() {
        return Err(format!("source[{i}].name must be non-empty"));
    }
    let prefix = format!("source[{i}]");

    let kind = match t.get("kind").and_then(toml::Value::as_str) {
        Some("files") => SourceKind::Files,
        Some("sqlite") => SourceKind::Sqlite,
        Some("whole") => SourceKind::Whole,
        Some(_) => return Err(format!("{prefix}.kind is not one of files|sqlite|whole")),
        None => return Err(format!("{prefix}.kind is required")),
    };

    let converter: Vec<String> = match t.get("converter").and_then(toml::Value::as_array) {
        Some(arr) if !arr.is_empty() => {
            let mut out = Vec::with_capacity(arr.len());
            for (j, v) in arr.iter().enumerate() {
                let s = v
                    .as_str()
                    .ok_or_else(|| format!("{prefix}.converter[{j}] must be a string"))?;
                if s.is_empty() {
                    return Err(format!("{prefix}.converter[{j}] must be non-empty"));
                }
                out.push(s.to_string());
            }
            out
        }
        _ => return Err(format!("{prefix}.converter must be a non-empty argv array")),
    };

    let device_id = required_token(t, "device_id", &prefix)?;
    let source_agent = required_token(t, "source_agent", &prefix)?;

    let project = match t.get("project") {
        None => "session".to_string(),
        Some(v) => {
            let s = v
                .as_str()
                .ok_or_else(|| format!("{prefix}.project must be a string"))?;
            if s == "session" {
                s.to_string()
            } else if let Some(rest) = s.strip_prefix("fixed:") {
                if rc_contract::safe_token(rest) != rest {
                    return Err(format!("{prefix}.project must be fixed:<safe_token>"));
                }
                s.to_string()
            } else {
                return Err(format!(
                    "{prefix}.project must be `session` or `fixed:<safe_token>`"
                ));
            }
        }
    };

    let root = opt_path(t, "root", &prefix)?;
    let glob = opt_str(t, "glob", &prefix)?;
    let db = opt_path(t, "db", &prefix)?;
    let cursor_sql = opt_str(t, "cursor_sql", &prefix)?;

    match kind {
        SourceKind::Files => {
            let root = root.ok_or_else(|| format!("{prefix}.root is required for files"))?;
            if !root.is_absolute() {
                return Err(format!("{prefix}.root must be an absolute path"));
            }
            if db.is_some() || cursor_sql.is_some() {
                return Err(format!("{prefix}: files source must not set db/cursor_sql"));
            }
            let glob = glob.ok_or_else(|| format!("{prefix}.glob is required for files"))?;
            check_glob(&glob, &prefix)?;
            Ok(Source {
                name,
                kind,
                root: Some(root),
                glob: Some(glob),
                db: None,
                cursor_sql: None,
                converter,
                device_id,
                source_agent,
                project,
            })
        }
        SourceKind::Whole => {
            let root = root.ok_or_else(|| format!("{prefix}.root is required for whole"))?;
            if !root.is_absolute() {
                return Err(format!("{prefix}.root must be an absolute path"));
            }
            if db.is_some() || cursor_sql.is_some() {
                return Err(format!("{prefix}: whole source must not set db/cursor_sql"));
            }
            let glob = glob.ok_or_else(|| format!("{prefix}.glob is required for whole"))?;
            check_glob(&glob, &prefix)?;
            if glob.contains("**") {
                return Err(format!("{prefix}.glob must not contain `**`"));
            }
            Ok(Source {
                name,
                kind,
                root: Some(root),
                glob: Some(glob),
                db: None,
                cursor_sql: None,
                converter,
                device_id,
                source_agent,
                project,
            })
        }
        SourceKind::Sqlite => {
            if root.is_some() || glob.is_some() {
                return Err(format!("{prefix}: sqlite source must not set root/glob"));
            }
            let db = db.ok_or_else(|| format!("{prefix}.db is required for sqlite"))?;
            let cursor_sql =
                cursor_sql.ok_or_else(|| format!("{prefix}.cursor_sql is required for sqlite"))?;
            check_cursor_sql(&cursor_sql, &prefix)?;
            Ok(Source {
                name,
                kind,
                root: None,
                glob: None,
                db: Some(db),
                cursor_sql: Some(cursor_sql),
                converter,
                device_id,
                source_agent,
                project,
            })
        }
    }
}

/// `glob` 的通用形状检查：非空、不许 `..`（不许越出 root）。
///
/// 多段通配（`*/*/session.v4.jsonl.zstd`）由扫描层逐段匹配；`**` 不递归通配。
fn check_glob(glob: &str, prefix: &str) -> Result<(), String> {
    if glob.is_empty() {
        return Err(format!("{prefix}.glob must be a non-empty pattern"));
    }
    if glob.contains("..") {
        return Err(format!("{prefix}.glob must not contain `..`"));
    }
    Ok(())
}

/// `cursor_sql` 只允许一条 SELECT：多语句会让我们在只读连接上执行到预期之外的东西。
fn check_cursor_sql(sql: &str, prefix: &str) -> Result<(), String> {
    let trimmed = sql.trim();
    let body = trimmed.strip_suffix(';').unwrap_or(trimmed);
    if !body.trim_start().to_ascii_lowercase().starts_with("select") {
        return Err(format!("{prefix}.cursor_sql must start with SELECT"));
    }
    if body.contains(';') {
        return Err(format!("{prefix}.cursor_sql must be a single statement"));
    }
    Ok(())
}

/// `safe_token` 用 `(redacted)` 当失败哨兵；相等即通过，错误串不回显原值。
fn required_token(t: &toml::Table, key: &str, prefix: &str) -> Result<String, String> {
    let s = t
        .get(key)
        .and_then(toml::Value::as_str)
        .ok_or_else(|| format!("{prefix}.{key} is required"))?;
    if rc_contract::safe_token(s) != s {
        return Err(format!("{prefix}.{key} is not a safe token"));
    }
    Ok(s.to_string())
}

fn opt_str(t: &toml::Table, key: &str, prefix: &str) -> Result<Option<String>, String> {
    match t.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Some(s.to_string()))
            .ok_or_else(|| format!("{prefix}.{key} must be a string")),
    }
}

fn opt_path(t: &toml::Table, key: &str, prefix: &str) -> Result<Option<PathBuf>, String> {
    match opt_str(t, key, prefix)? {
        None => Ok(None),
        Some(s) if s.is_empty() => Err(format!("{prefix}.{key} must be non-empty")),
        Some(s) => Ok(Some(PathBuf::from(s))),
    }
}
