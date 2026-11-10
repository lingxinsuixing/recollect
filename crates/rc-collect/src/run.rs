//! `run_once`：一轮采集的完整流程——短锁、扫描、切段、调转换器、回执、
//! 推进水位、日帽与毒化。
//!
//! 三条纪律（设计 §6.1/§6.3）：
//!
//! 1. **只按回执推进水位**：`batch_reject` 出现 ⇒ 不推进、attempts+1，连续 3 次即
//!    标 poisoned；sink 报运维失败（`Err`）⇒ 不推进、不计数毒化。
//! 2. **offset 只落在完整行边界**：段内最后一个 `\n` 之后；末行没有 `\n` 时，
//!    只有整段本身是一份完整 JSON 才越过。
//! 3. **两道帽全局跨源、按日计**：`max_files_per_run` 与 `max_turns_per_day`
//!    触顶后本轮不再开始新文件；次日继续，不丢数据。
//!
//! `dry_run` 只扫描、不执行转换器、不提交；`new_turns` 不做估算，记 0。
//! 日志（`$RECOLLECT_LOG`）零正文、零绝对路径：只印源名、计数与错误类别。

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::config::{Config, Source, SourceKind};
use crate::scan::scan_sqlite;
use crate::scan::{head_sha256, path_key, scan_files, DueFile};
use crate::state::{DailyUse, FileCursor, SqliteCursor, StateDoc};

/// 一次提交的回执。`batch_reject = Some(code)` 表示整批契约拒收。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Receipt {
    pub accepted: usize,
    pub duplicate: usize,
    pub rejected: usize,
    pub batch_reject: Option<String>,
}

/// 提交侧（本机 = `hub import`；边缘 = `/v1/ingest`）。`rc-collect` 不碰数据库。
pub trait Sink {
    fn submit(&mut self, v1_path: &Path) -> Result<Receipt, String>;
}

#[derive(Debug, Clone, Default)]
pub struct RunOpts {
    /// 只跑某个源；`None` 或 `Some("all")` = 全部。
    pub only_source: Option<String>,
    pub dry_run: bool,
    /// 人工清毒化标记后重试。
    pub retry_poisoned: bool,
    /// sqlite 源首跑从 0 起回填（缺省只收未来：把水位钉到库内当前最大值）。
    pub backfill: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SourceReport {
    pub source: String,
    pub due: usize,
    pub new_turns: usize,
    pub accepted: usize,
    pub duplicate: usize,
    pub rejected: usize,
    pub advanced: bool,
    pub reason: String,
}

/// 跑一轮采集。`now_ms` 是唯一时钟来源。
///
/// `Err` 只用于状态文件不可读/不可写；单源或单文件的失败记进 `reason` 并继续。
pub fn run_once(
    cfg: &Config,
    state_path: &Path,
    sink: &mut dyn Sink,
    now_ms: i64,
    opts: &RunOpts,
) -> Result<Vec<SourceReport>, String> {
    let state_dir = state_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    std::fs::create_dir_all(&state_dir)
        .map_err(|_| "collect state dir is not creatable".to_string())?;

    // 短锁：已有活进程在跑就安静退出（本机并发是浪费，不是错误）。
    let _lock = match crate::lock::try_lock(&state_dir)? {
        Some(l) => l,
        None => return Ok(Vec::new()),
    };

    let mut st = StateDoc::load(state_path)?;

    // 日帽：全局一份，滚动 24 小时窗口（与本地时区无关，跨 UTC 午夜不会把昨天用掉的额度带进今天）。
    let day = "window".to_string();
    let cap = cfg.limits.max_turns_per_day;
    let mut dirty = false;
    match st.daily.get_mut(&day) {
        Some(d) => {
            let expired = d.start_ms == 0 || now_ms.saturating_sub(d.start_ms) >= DAY_MS;
            if d.cap != cap || expired {
                d.cap = cap;
                d.used = 0;
                d.start_ms = now_ms;
                dirty = true;
            }
        }
        None => {
            st.daily.insert(day.clone(), DailyUse { cap, used: 0, start_ms: now_ms });
            dirty = true;
        }
    }

    let mut reports: Vec<SourceReport> = Vec::new();
    let mut files_started: u32 = 0;
    let mut sqlite_started: u32 = 0;
    let mut file_budget_hit = false;

    for src in &cfg.sources {
        if let Some(only) = &opts.only_source {
            if only != "all" && only != &src.name {
                continue;
            }
        }

        let mut rep = SourceReport {
            source: src.name.clone(),
            reason: "idle".to_string(),
            ..Default::default()
        };

        if src.kind == SourceKind::Sqlite {
            sqlite_started += 1;
            let (srep, changed) = run_sqlite_source(
                &mut st,
                src,
                &state_dir,
                cfg.limits.quiet_ms,
                cfg.limits.max_turns_per_day,
                now_ms,
                opts,
                &day,
                sqlite_started,
                sink,
            );
            dirty |= changed;
            push_report(&mut reports, srep);
            continue;
        }

        let due = scan_files(src, &st, cfg.limits.quiet_ms, now_ms);
        rep.due = due.len();

        if opts.dry_run {
            rep.reason = "dry_run".to_string();
            push_report(&mut reports, rep);
            continue;
        }
        if due.is_empty() {
            push_report(&mut reports, rep);
            continue;
        }
        if file_budget_hit {
            rep.reason = "cap_files".to_string();
            push_report(&mut reports, rep);
            continue;
        }

        for f in &due {
            if files_started >= cfg.limits.max_files_per_run {
                file_budget_hit = true;
                rep.reason = "cap_files".to_string();
                break;
            }
            let used_now = st.daily.get(&day).map(|d| d.used).unwrap_or(0);
            if used_now >= cap {
                rep.reason = "cap_turns".to_string();
                break;
            }

            let key = path_key(&f.path);
            if let Some(p) = st.poisoned.get(&key) {
                if opts.retry_poisoned {
                    st.poisoned.remove(&key);
                    dirty = true;
                } else if p.attempts >= 3 {
                    rep.reason = "poisoned".to_string();
                    continue;
                }
            }

            files_started += 1;
            let outcome = process_file(&mut st, src, f, &state_dir, files_started, &day, sink);

            rep.new_turns += outcome.new_turns;
            rep.accepted += outcome.accepted;
            rep.duplicate += outcome.duplicate;
            rep.rejected += outcome.rejected;
            rep.advanced |= outcome.advanced;
            if outcome.reason != "ok" {
                rep.reason = outcome.reason;
            } else if rep.reason == "idle" {
                rep.reason = "ok".to_string();
            }

            if outcome.advanced || outcome.state_changed {
                dirty = true;
            }
        }

        push_report(&mut reports, rep);
    }

    if dirty {
        st.save(state_path)?;
    }
    Ok(reports)
}

/// 数一份 v1 里 human/agent 轮的数量（多 session 文件累加）。
pub fn human_agent_turns(v1_path: &Path) -> Result<usize, String> {
    let text = std::fs::read_to_string(v1_path).map_err(|_| "v1 is unreadable".to_string())?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|_| "v1 is not valid JSON".to_string())?;
    let sessions = v
        .get("sessions")
        .and_then(|s| s.as_array())
        .ok_or_else(|| "v1 has no sessions".to_string())?;
    let mut n = 0usize;
    for s in sessions {
        if let Some(turns) = s.get("turns").and_then(|t| t.as_array()) {
            for t in turns {
                match t.get("role").and_then(|r| r.as_str()) {
                    Some("human") | Some("agent") => n += 1,
                    _ => {}
                }
            }
        }
    }
    Ok(n)
}

/// UTC 天编号（自 epoch 起的整天数）。跨日即换帽。
/// 日帽窗口长度：滚动 24 小时，不做日历日切分（无时区依赖）。
pub const DAY_MS: i64 = 86_400_000;


#[derive(Debug, Default)]
struct SourceOutcome {
    advanced: bool,
    /// 状态文件里有改动但不是水位推进（今天只有毒化计数）。
    state_changed: bool,
    new_turns: usize,
    accepted: usize,
    duplicate: usize,
    rejected: usize,
    reason: String,
}

/// sqlite 源在 `poisoned` 里的键：加前缀，免得和文件路径键撞。
fn sqlite_key(name: &str) -> String {
    format!("sqlite:{name}")
}

/// 处理一个 sqlite 源（设计 §5）：
///
/// 1. `scan_sqlite` 给 `latest_ms` 与水位；没有新行或还在静默窗口内 ⇒ `idle`；
/// 2. 首跑没有水位 ⇒ 只收未来：把水位钉到 `latest_ms`，`reason="seeded"`，不转换；
///    `opts.backfill` 才从 0 起回填；
/// 3. 否则以 `{since_ms}=水位` 调转换器，回执成功后才把水位推进到 `latest_ms`。
///
/// 失败一律落在 `reason`，绝不推进水位；sink 运维失败不计毒化，`batch_reject` 才计。
#[allow(clippy::too_many_arguments)]
fn run_sqlite_source(
    st: &mut StateDoc,
    src: &Source,
    state_dir: &Path,
    quiet_ms: i64,
    cap: u32,
    now_ms: i64,
    opts: &RunOpts,
    day: &str,
    seq: u32,
    sink: &mut dyn Sink,
) -> (SourceReport, bool) {
    let mut rep = SourceReport {
        source: src.name.clone(),
        reason: "idle".to_string(),
        ..Default::default()
    };

    let due = match scan_sqlite(src, st, quiet_ms, now_ms) {
        Ok(Some(d)) => d,
        Ok(None) => return (rep, false),
        Err(_) => {
            rep.reason = "sqlite_failed".to_string();
            return (rep, false);
        }
    };
    rep.due = 1;

    if opts.dry_run {
        rep.reason = "dry_run".to_string();
        return (rep, false);
    }

    if !st.sqlite.contains_key(&src.name) && !opts.backfill {
        st.sqlite.insert(
            src.name.clone(),
            SqliteCursor {
                last_created_at_ms: due.latest_ms,
                last_msg_id: None,
            },
        );
        rep.reason = "seeded".to_string();
        push_sqlite_log(src, &rep);
        return (rep, true);
    }

    let key = sqlite_key(&src.name);
    if let Some(p) = st.poisoned.get(&key) {
        if opts.retry_poisoned {
            st.poisoned.remove(&key);
        } else if p.attempts >= 3 {
            rep.reason = "poisoned".to_string();
            return (rep, true);
        }
    }

    let used_now = st.daily.get(day).map(|d| d.used).unwrap_or(0);
    if used_now >= cap {
        rep.reason = "cap_turns".to_string();
        return (rep, false);
    }

    let tmp = state_dir
        .join("tmp")
        .join(format!("{}-sqlite-{}", std::process::id(), seq));
    let out_dir = tmp.join("out");

    let mut run = || -> Result<SourceOutcome, String> {
        std::fs::create_dir_all(&out_dir).map_err(|_| "tmp_failed".to_string())?;

        // sqlite 源没有"段"：`{segment}` 替换成临时目录（转换器不该读它）。
        let (v1, new_turns) = invoke_converter(src, &tmp, &out_dir, due.since_ms)?;

        let receipt = sink.submit(&v1).map_err(|_| "sink_failed".to_string())?;
        let mut out = SourceOutcome {
            new_turns,
            accepted: receipt.accepted,
            duplicate: receipt.duplicate,
            rejected: receipt.rejected,
            reason: "ok".to_string(),
            advanced: false,
            state_changed: false,
        };

        if let Some(code) = receipt.batch_reject {
            let p = st.poisoned.entry(key.clone()).or_default();
            p.attempts += 1;
            p.last_code = code.clone();
            out.reason = format!("batch_reject:{code}");
            out.state_changed = true;
            return Ok(out);
        }

        st.poisoned.remove(&key);
        st.sqlite.insert(
            src.name.clone(),
            SqliteCursor {
                last_created_at_ms: due.latest_ms,
                last_msg_id: None,
            },
        );
        if let Some(d) = st.daily.get_mut(day) {
            d.used = d
                .used
                .saturating_add(u32::try_from(new_turns).unwrap_or(u32::MAX));
        }
        out.advanced = true;
        Ok(out)
    };

    let out = match run() {
        Ok(o) => o,
        Err(reason) => SourceOutcome {
            reason,
            ..Default::default()
        },
    };
    let _ = std::fs::remove_dir_all(&tmp);

    rep.new_turns = out.new_turns;
    rep.accepted = out.accepted;
    rep.duplicate = out.duplicate;
    rep.rejected = out.rejected;
    rep.advanced = out.advanced;
    rep.reason = out.reason;
    push_sqlite_log(src, &rep);
    (rep, out.advanced || out.state_changed)
}

/// sqlite 源的逐源日志（与 files 的 `push_report` 同形，另加 `kind`）。
fn push_sqlite_log(src: &Source, rep: &SourceReport) {
    log_event(&serde_json::json!({
        "t": "collect",
        "scope": "source",
        "kind": "sqlite",
        "source": src.name,
        "due": rep.due,
        "new_turns": rep.new_turns,
        "accepted": rep.accepted,
        "duplicate": rep.duplicate,
        "rejected": rep.rejected,
        "advanced": rep.advanced,
        "reason": rep.reason,
    }));
}

/// 处理一个 due 文件。任何失败都落在 `reason`，绝不推进水位。
fn process_file(
    st: &mut StateDoc,
    src: &Source,
    due: &DueFile,
    state_dir: &Path,
    seq: u32,
    day: &str,
    sink: &mut dyn Sink,
) -> SourceOutcome {
    let key = path_key(&due.path);
    let tmp = state_dir
        .join("tmp")
        .join(format!("{}-{}", std::process::id(), seq));
    let out_dir = tmp.join("out");

    let mut run = || -> Result<SourceOutcome, String> {
        std::fs::create_dir_all(&out_dir).map_err(|_| "tmp_failed".to_string())?;

        let segment = read_segment(&due.path, due.offset)?;
        let stem = due
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "segment".to_string());
        let seg_path = tmp.join(format!("{stem}.jsonl"));
        std::fs::write(&seg_path, &segment).map_err(|_| "tmp_failed".to_string())?;

        let (v1, new_turns) = invoke_converter(src, &seg_path, &out_dir, 0)?;

        let receipt = sink.submit(&v1).map_err(|_| "sink_failed".to_string())?;
        let mut out = SourceOutcome {
            new_turns,
            accepted: receipt.accepted,
            duplicate: receipt.duplicate,
            rejected: receipt.rejected,
            reason: "ok".to_string(),
            advanced: false,
            state_changed: false,
        };

        if let Some(code) = receipt.batch_reject {
            // 整批拒收：不推进，attempts+1；≥3 即 poisoned（同一 map 里表态）。
            let p = st.poisoned.entry(key.clone()).or_default();
            p.attempts += 1;
            p.last_code = code.clone();
            out.reason = format!("batch_reject:{code}");
            out.state_changed = true;
            return Ok(out);
        }

        st.poisoned.remove(&key);

        // offset 只落在完整行边界。
        let (consumed, complete) = match segment.iter().rposition(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => {
                if serde_json::from_slice::<serde_json::Value>(&segment).is_ok() {
                    (segment.len(), true)
                } else {
                    (0, false)
                }
            }
        };
        if !complete {
            out.reason = "partial_segment".to_string();
            return Ok(out);
        }

        let new_offset = due.offset + consumed as u64;
        let head = head_sha256(&due.path).unwrap_or_default();
        st.files.insert(
            key.clone(),
            FileCursor {
                mtime_ms: due.mtime_ms,
                size: due.size as i64,
                offset: new_offset as i64,
                last_turns: due.last_turns + new_turns as i64,
                head_sha256: head,
            },
        );
        if let Some(d) = st.daily.get_mut(day) {
            d.used = d
                .used
                .saturating_add(u32::try_from(new_turns).unwrap_or(u32::MAX));
        }
        out.advanced = true;
        Ok(out)
    };

    let out = match run() {
        Ok(o) => o,
        Err(reason) => SourceOutcome {
            reason,
            ..Default::default()
        },
    };
    let _ = std::fs::remove_dir_all(&tmp);
    log_event(&serde_json::json!({
        "t": "collect",
        "scope": "file",
        "source": src.name,
        "new_turns": out.new_turns,
        "accepted": out.accepted,
        "duplicate": out.duplicate,
        "rejected": out.rejected,
        "advanced": out.advanced,
        "reason": out.reason,
    }));
    out
}

/// 执行转换器并取它产出的**恰好一份** v1，同时数出 human/agent 轮。
///
/// argv 里的占位符替换后直接 exec，不经 shell。返回 (v1 路径, human/agent 轮数)。
fn invoke_converter(
    src: &Source,
    segment: &Path,
    out_dir: &Path,
    since_ms: i64,
) -> Result<(PathBuf, usize), String> {
    let argv = expand_argv(&src.converter, segment, out_dir, since_ms, &src.name);
    if argv.is_empty() {
        return Err("converter_empty".to_string());
    }
    let output = std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|_| "converter_spawn".to_string())?;
    if !output.status.success() {
        return Err(format!(
            "converter_failed:{}",
            output.status.code().unwrap_or(-1)
        ));
    }

    let mut json = list_v1(out_dir);
    if json.len() != 1 {
        // 必须是"恰好一个 *.json"：0 个或多义都算转换器违约。
        return Err("converter_output".to_string());
    }
    let v1 = json.pop().unwrap();
    let new_turns = human_agent_turns(&v1).map_err(|_| "v1_unreadable".to_string())?;
    Ok((v1, new_turns))
}

fn read_segment(path: &Path, offset: u64) -> Result<Vec<u8>, String> {
    let mut f = std::fs::File::open(path).map_err(|_| "source_unreadable".to_string())?;
    f.seek(SeekFrom::Start(offset))
        .map_err(|_| "source_unreadable".to_string())?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)
        .map_err(|_| "source_unreadable".to_string())?;
    Ok(buf)
}

fn expand_argv(
    argv: &[String],
    segment: &Path,
    out: &Path,
    since_ms: i64,
    source: &str,
) -> Vec<String> {
    let segment = segment.to_string_lossy();
    let out = out.to_string_lossy();
    let since = since_ms.to_string();
    argv.iter()
        .map(|a| {
            a.replace("{segment}", &segment)
                .replace("{out}", &out)
                .replace("{since_ms}", &since)
                .replace("{source}", source)
        })
        .collect()
}

/// 在 out 目录里找转换器产物。约定优先 `*.v1.json`（Qoder 转换器还会写一份
/// `<stem>.manifest.json`，按“恰好一个 json”判会把正常产物判成违约）。
/// 没有 v1 命名时退回“恰好一个 json”。
fn list_v1(out_dir: &Path) -> Vec<PathBuf> {
    let mut all = Vec::new();
    let rd = match std::fs::read_dir(out_dir) {
        Ok(r) => r,
        Err(_) => return all,
    };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_file() && p.extension().map(|e| e == "json").unwrap_or(false) {
            all.push(p);
        }
    }
    all.sort();
    let v1: Vec<PathBuf> = all
        .iter()
        .filter(|p| p.to_string_lossy().ends_with(".v1.json"))
        .cloned()
        .collect();
    match v1.len() {
        0 => all,
        _ => v1,
    }
}

fn push_report(reports: &mut Vec<SourceReport>, rep: SourceReport) {
    log_event(&serde_json::json!({
        "t": "collect",
        "scope": "source",
        "source": rep.source,
        "due": rep.due,
        "new_turns": rep.new_turns,
        "accepted": rep.accepted,
        "duplicate": rep.duplicate,
        "rejected": rep.rejected,
        "advanced": rep.advanced,
        "reason": rep.reason,
    }));
    reports.push(rep);
}

/// 只往 `$RECOLLECT_LOG` 追加一行 JSON；未设置或打不开就安静跳过。
fn log_event(value: &serde_json::Value) {
    let path = match std::env::var("RECOLLECT_LOG") {
        Ok(p) if !p.is_empty() => p,
        _ => return,
    };
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "{value}");
    }
}


#[cfg(test)]
mod tests {
    use super::list_v1;

    #[test]
    fn v1_named_output_wins_over_manifest() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.v1.json"), "{}").unwrap();
        std::fs::write(d.path().join("a.manifest.json"), "{}").unwrap();
        let got = list_v1(d.path());
        assert_eq!(got.len(), 1);
        assert!(got[0].to_string_lossy().ends_with("a.v1.json"));
    }

    #[test]
    fn two_v1_outputs_stay_ambiguous() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.v1.json"), "{}").unwrap();
        std::fs::write(d.path().join("b.v1.json"), "{}").unwrap();
        assert_eq!(list_v1(d.path()).len(), 2);
    }

    #[test]
    fn lone_json_without_v1_suffix_is_accepted() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("mcode.json"), "{}").unwrap();
        assert_eq!(list_v1(d.path()).len(), 1);
    }
}
