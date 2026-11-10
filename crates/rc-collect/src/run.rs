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
use crate::scan::{head_sha256, path_key, scan_files, scan_whole, DueFile, WholeDue};
use crate::state::{
    FileCursor, SqliteCursor, StateDoc, WholeCursor, BUDGET_BACKFILL, BUDGET_WINDOW,
};

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

/// 单轮回填的轮数上限（缺省 2000）。它**只管单轮**：触顶 ⇒ 本轮停止、剩余顺延到下一轮。
///
/// 24 小时的回填预算是另一只旋钮：`[limits] max_backfill_turns_per_day`
/// （见 [`crate::config::Limits`]），与日常额度共用同一套滚动窗口过期机制。
pub const DEFAULT_MAX_BACKFILL_TURNS: u32 = 2000;

/// 蒸馏积压背压阈值（缺省 20000）。超过就拒绝本轮回填（这一轮没做事，不是故障）。
pub const DEFAULT_MAX_BACKFILL_PENDING: u32 = 20_000;

#[derive(Debug, Clone)]
pub struct RunOpts {
    /// 只跑某个源；`None` 或 `Some("all")` = 全部。
    pub only_source: Option<String>,
    pub dry_run: bool,
    /// 人工清毒化标记后重试。
    pub retry_poisoned: bool,
    /// 本轮允许历史回填：让**尚无水位**的 sqlite 源从 0 全量灌（缺省是 seed，只收未来）。
    pub backfill: bool,
    /// 连已有水位的源也回填（`since_ms=0`，幂等但昂贵）。默认关。
    pub backfill_force: bool,
    /// **单轮**回填的轮数上限（触顶 ⇒ 本轮停止、剩余顺延）。24 小时的回填预算是另一只
    /// 旋钮：`[limits] max_backfill_turns_per_day`。
    pub max_backfill_turns: u32,
    /// 蒸馏积压背压阈值；`None` = 不设闸。
    pub max_backfill_pending: Option<u32>,
    /// 当前蒸馏积压（轮）。`None` = 读不到 ⇒ 按通过处理（不因为读不到就拒绝采集）。
    pub pending_turns: Option<i64>,
}

impl Default for RunOpts {
    fn default() -> Self {
        RunOpts {
            only_source: None,
            dry_run: false,
            retry_poisoned: false,
            backfill: false,
            backfill_force: false,
            max_backfill_turns: DEFAULT_MAX_BACKFILL_TURNS,
            max_backfill_pending: None,
            pending_turns: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceReport {
    pub source: String,
    pub due: usize,
    pub new_turns: usize,
    pub accepted: usize,
    pub duplicate: usize,
    pub rejected: usize,
    pub advanced: bool,
    pub reason: String,
    /// 本轮走的是哪条预算：`window`（日常增量）或 `backfill`（历史回填）。
    pub budget: String,
    /// 拒绝回填时的蒸馏积压（轮）；`None` = 没拒过或读不到积压。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_turns: Option<i64>,
}

impl Default for SourceReport {
    fn default() -> Self {
        SourceReport {
            source: String::new(),
            due: 0,
            new_turns: 0,
            accepted: 0,
            duplicate: 0,
            rejected: 0,
            advanced: false,
            reason: String::new(),
            budget: BUDGET_WINDOW.to_string(),
            pending_turns: None,
        }
    }
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

    // 两条预算：日常增量（`window`，所有源共用）与历史回填（`backfill`，专用）。
    // 都是滚动 24 小时窗口；回填轮不吃日常额度，日常轮不吃回填额度。
    let cap = cfg.limits.max_turns_per_day;
    let (mut win_used, changed) = st.budget_used(BUDGET_WINDOW, cap, now_ms);
    let mut dirty = changed;
    // 24 小时回填预算：上限来自配置 `[limits] max_backfill_turns_per_day`，
    // 与日常额度共用同一套过期机制（滚动窗口）。开轮时结算：过期就归零。
    let backfill_cap = cfg.limits.max_backfill_turns_per_day;
    let (_, changed) = st.budget_used(BUDGET_BACKFILL, backfill_cap, now_ms);
    dirty |= changed;

    let mut reports: Vec<SourceReport> = Vec::new();
    let mut files_started: u32 = 0;
    let mut sqlite_started: u32 = 0;
    let mut file_budget_hit = false;
    // 本轮已用掉的回填轮数：单轮回填上限（触顶后本轮停止，剩余顺延）。
    let mut backfill_turns: u32 = 0;

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
                backfill_cap,
                now_ms,
                opts,
                &mut backfill_turns,
                sqlite_started,
                sink,
            );
            dirty |= changed;
            // sqlite 源也可能花日常额度（日常增量那一支）：刷新本地读数，
            // 否则后面的 files 源会拿旧的用量去过帽。
            win_used = st.daily.get(BUDGET_WINDOW).map(|d| d.used).unwrap_or(0);
            push_report(&mut reports, srep);
            continue;
        }

        if src.kind == SourceKind::Whole {
            // 整文件源：整份文件交给转换器，转换器自己读它、写 v1 到 `{out}`。
            let due = scan_whole(src, &st, cfg.limits.quiet_ms, now_ms);
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
                if win_used >= cap {
                    rep.reason = "cap_turns".to_string();
                    break;
                }

                let key = whole_poison_key(&f.path);
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
                let outcome = process_whole_file(&mut st, src, f, &state_dir, files_started, sink);

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
                if outcome.advanced {
                    // 整文件源的日帽按**真正入库的新轮**（回执 accepted）计：
                    // 整份重转会连旧轮一起重发（hub 按内容哈希吸收成 duplicate），
                    // 按产出轮数计会把同一批轮反复扣额度。
                    let charged = outcome.accepted as u32;
                    st.budget_spend(BUDGET_WINDOW, charged);
                    win_used = win_used.saturating_add(charged);
                }
            }

            push_report(&mut reports, rep);
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
            if win_used >= cap {
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
            let outcome = process_file(&mut st, src, f, &state_dir, files_started, sink);

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
            if outcome.advanced {
                st.budget_spend(BUDGET_WINDOW, outcome.new_turns as u32);
                win_used = win_used.saturating_add(outcome.new_turns as u32);
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

/// 日帽窗口长度：滚动 24 小时，不做日历日切分（无时区依赖）。
/// 住在 `state` 里：两条预算的窗口长度是同一个数。
pub use crate::state::DAY_MS;


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
/// 1. `scan_sqlite` 给 `latest_ms`（库内当前最大值）与水位；没有新行或还在静默窗口内
///    ⇒ `idle`；
/// 2. 首跑没有水位 ⇒ 只收未来：把水位钉到 `latest_ms`，`reason="seeded"`，不转换；
///    `--backfill` 才从 0 起回填；
/// 3. 有水位 ⇒ 判「历史缺口」：水位落后**当前数据**（`cursor_sql` 的最大值）超过 24 小时
///    ⇒ 走 `backfill` 预算，从水位补到当前；否则走 `window` 预算（日常增量）；
/// 4. 以 `{since_ms}=水位`（或 0）调转换器，回执成功后才把水位推进。
///
/// 失败一律落在 `reason`，绝不推进水位；sink 运维失败不计毒化，`batch_reject` 才计。
#[allow(clippy::too_many_arguments)]
fn run_sqlite_source(
    st: &mut StateDoc,
    src: &Source,
    state_dir: &Path,
    quiet_ms: i64,
    cap: u32,
    backfill_cap: u32,
    now_ms: i64,
    opts: &RunOpts,
    backfill_turns: &mut u32,
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

    let watermark = st.sqlite.get(&src.name).map(|c| c.last_created_at_ms);
    let mut since_ms = due.since_ms;
    let mut is_backfill = false;

    match watermark {
        None => {
            if opts.backfill || opts.backfill_force {
                // 首跑全量：从 0 起。
                since_ms = 0;
                is_backfill = true;
            } else {
                // 只收未来：把水位钉到库内当前最大值。
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
        }
        Some(w) => {
            if opts.backfill_force {
                // 连已有水位的源也回填：从 0 起（幂等键吸收重复）。
                since_ms = 0;
                is_backfill = true;
            } else if is_historical(w, due.latest_ms) {
                // 水位落后「当前数据」超过一天：这一轮补的是历史（自动回填，
                // 不需要 `--backfill`；走 backfill 预算，受背压闸约束）。
                is_backfill = true;
            }
        }
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

    if is_backfill {
        rep.budget = BUDGET_BACKFILL.to_string();
        // 积压背压：读不到积压按通过处理；超阈值 ⇒ 本轮拒绝回填（这一轮没做事，不是故障）。
        if let (Some(limit), Some(pending)) = (opts.max_backfill_pending, opts.pending_turns) {
            if pending > i64::from(limit) {
                rep.reason = "backfill_blocked_pending".to_string();
                rep.pending_turns = Some(pending);
                push_sqlite_log(src, &rep);
                return (rep, false);
            }
        }
        // 单轮回填上限：触顶后本轮停止，剩余顺延到下一个窗口。
        if *backfill_turns >= opts.max_backfill_turns {
            rep.reason = "backfill_capped".to_string();
            push_sqlite_log(src, &rep);
            return (rep, false);
        }
        // 24 小时回填预算用尽 ⇒ 本轮拒绝回填，剩余等下一个滚动窗口。
        let used = st.daily.get(BUDGET_BACKFILL).map(|d| d.used).unwrap_or(0);
        if used >= backfill_cap {
            rep.reason = "backfill_budget_capped".to_string();
            push_sqlite_log(src, &rep);
            return (rep, false);
        }
    } else {
        let used = st.daily.get(BUDGET_WINDOW).map(|d| d.used).unwrap_or(0);
        if used >= cap {
            rep.reason = "cap_turns".to_string();
            return (rep, false);
        }
    }

    let tmp = state_dir
        .join("tmp")
        .join(format!("{}-sqlite-{}", std::process::id(), seq));
    let out_dir = tmp.join("out");

    let mut run = || -> Result<SourceOutcome, String> {
        std::fs::create_dir_all(&out_dir).map_err(|_| "tmp_failed".to_string())?;

        // sqlite 源没有"段"：`{segment}` 替换成临时目录（转换器不该读它）。
        let (v1, new_turns) = invoke_converter(src, &tmp, &out_dir, since_ms)?;

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
        // 水位：回填按"本轮实际收到的最大时间戳"推进（没收到的部分不能记成已收）；
        // 日常轮沿用库内最大值。
        let next_ms = if is_backfill {
            max_received_ms(&v1)
                .map(|m| m.max(since_ms))
                .unwrap_or(due.latest_ms)
        } else {
            due.latest_ms
        };
        st.sqlite.insert(
            src.name.clone(),
            SqliteCursor {
                last_created_at_ms: next_ms,
                last_msg_id: None,
            },
        );
        let turns = u32::try_from(new_turns).unwrap_or(u32::MAX);
        if is_backfill {
            st.budget_spend(BUDGET_BACKFILL, turns);
            *backfill_turns = backfill_turns.saturating_add(turns);
        } else {
            st.budget_spend(BUDGET_WINDOW, turns);
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
    // 回填成功与日常成功在回执里要能分开读：预算键只说"走哪条额度"，
    // reason 才说"这一轮做的是历史回填"。
    rep.reason = match (is_backfill, out.reason.as_str()) {
        (true, "ok") => "backfill".to_string(),
        _ => out.reason,
    };
    push_sqlite_log(src, &rep);
    (rep, out.advanced || out.state_changed)
}

/// 判一个 sqlite 源这轮算「历史回填」还是「日常增量」。
///
/// 判据：**水位落后「当前数据」超过 24 小时**。`max_ms` 就是 `cursor_sql` 取到的
/// 库内当前最大时间戳（不再需要第二次查询取 min）：`max_ms - watermark > 24h`
/// ⇒ 这一轮补的是历史，走 `backfill` 预算；否则 ⇒ 日常增量，走 `window` 预算。
///
/// 读不出 `max_ms` 的源压根到不了这一步（`scan_sqlite` 已经把它滤掉了），
/// 所以这里没有"读不出来就按历史"的兜底分支。
fn is_historical(watermark_ms: i64, max_ms: i64) -> bool {
    max_ms.saturating_sub(watermark_ms) > DAY_MS
}

/// v1 里实际收到的最大 `created_at_ms`。
///
/// 回填按它推进水位：没收到的部分不能记成已收，否则那段历史永远补不上。
fn max_received_ms(v1_path: &Path) -> Option<i64> {
    let text = std::fs::read_to_string(v1_path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let sessions = v.get("sessions")?.as_array()?;
    let mut max: Option<i64> = None;
    for s in sessions {
        let turns = match s.get("turns").and_then(|t| t.as_array()) {
            Some(t) => t,
            None => continue,
        };
        for t in turns {
            if let Some(ms) = t.get("created_at_ms").and_then(|m| m.as_i64()) {
                max = Some(match max {
                    Some(cur) if cur >= ms => cur,
                    _ => ms,
                });
            }
        }
    }
    max
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
        "budget": rep.budget,
        "reason": rep.reason,
    }));
}

/// 整文件源在 `poisoned` 里的键：加前缀，免得和 files 源的路径键撞。
fn whole_poison_key(path: &Path) -> String {
    format!("whole:{}", path_key(path))
}

/// 处理一个整文件源的文件（设计 §5 的整文件变体）。
///
/// 与 files 源的三条差别：
/// 1. **`{segment}` 是源文件的原始路径**：不复制、不切段——压缩流没有可切分的边界，
///    转换器自己读它、写 v1 到 `{out}`。
/// 2. **水位记 `{mtime_ms, size, head_sha256}`**：没有 offset 这一格；文件变了就整份重转。
/// 3. **回执的 `accepted` 才是新增轮数**：整份重转时旧轮会被幂等键吸收成 `duplicate`，
///    所以不能拿 v1 里的轮数当"新增"（那会把日帽算重）。
///
/// 失败一律落在 `reason`，绝不推进水位。
fn process_whole_file(
    st: &mut StateDoc,
    src: &Source,
    due: &WholeDue,
    state_dir: &Path,
    seq: u32,
    sink: &mut dyn Sink,
) -> SourceOutcome {
    let key = path_key(&due.path);
    let poison_key = whole_poison_key(&due.path);
    let tmp = state_dir
        .join("tmp")
        .join(format!("{}-whole-{}", std::process::id(), seq));
    let out_dir = tmp.join("out");

    let mut run = || -> Result<SourceOutcome, String> {
        std::fs::create_dir_all(&out_dir).map_err(|_| "tmp_failed".to_string())?;

        // 整文件源：把**源文件路径本身**交给转换器（`{segment}`），转换器自己读。
        let (v1, produced) = invoke_converter(src, &due.path, &out_dir, 0)?;

        let receipt = sink.submit(&v1).map_err(|_| "sink_failed".to_string())?;
        let mut out = SourceOutcome {
            // 与其它源同口径：v1 里的 human+agent 轮数。
            // 整份重转时旧轮会被幂等键吸收成 `duplicate`（回执里能看到），
            // 但日帽按"产出轮数"计 ⇒ 宁可早停，不让积压失控。
            new_turns: produced,
            accepted: receipt.accepted,
            duplicate: receipt.duplicate,
            rejected: receipt.rejected,
            reason: "ok".to_string(),
            advanced: false,
            state_changed: false,
        };

        if let Some(code) = receipt.batch_reject {
            let p = st.poisoned.entry(poison_key.clone()).or_default();
            p.attempts += 1;
            p.last_code = code.clone();
            out.reason = format!("batch_reject:{code}");
            out.state_changed = true;
            return Ok(out);
        }

        st.poisoned.remove(&poison_key);

        // 水位推进：{mtime_ms, size, head_sha256}（offset 恒 0 ⇒ 没有这一格）。
        // 记的是**扫描时**看到的那一份：扫描与转换之间文件又变了 ⇒ 下一轮再转一次，
        // 宁可多转一次也不漏。
        let last_turns = st.whole.get(&key).map(|c| c.last_turns).unwrap_or(0);
        st.whole.insert(
            key.clone(),
            WholeCursor {
                mtime_ms: due.mtime_ms,
                size: due.size as i64,
                head_sha256: due.head_sha256.clone(),
                last_turns: last_turns + out.new_turns as i64,
            },
        );
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
        "kind": "whole",
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

/// 处理一个 due 文件。任何失败都落在 `reason`，绝不推进水位。
fn process_file(
    st: &mut StateDoc,
    src: &Source,
    due: &DueFile,
    state_dir: &Path,
    seq: u32,
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
        "budget": rep.budget,
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
