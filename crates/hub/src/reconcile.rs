//! H7c1：接管对账（`hub reconcile memory`）。**只在缺口内动状态行**。
//!
//! 接管语义：standby 的记忆可以落后一周，但转录轴（`center_state.last_commit_seq`）是新的；
//! 接管后"哪些轮记忆缺"不能靠猜，要从两条水位算出来。这一层只做两件事：
//!
//! 1. **量缺口**：`(memory_commit_seq, transcript_commit_seq]` 里有多少轮、这些轮的 pass 行
//!    按状态怎么分布（`by_state`）、其中多少轮是 `done`（`done_turns`）。
//! 2. **重蒸**（仅 `--apply`）：把缺口内 `state=done` 的**每一条 pass 行**改成
//!    `stale(reason=memory_gap)`——"已标 done 却缺行"的轮只能靠重跑抽取补上，
//!    而 `should_run` 见 `stale` 就点头。其余状态一律不动：`empty/rejected/quarantined`
//!    本来就没有可缺的行，`pending/running` 还没产出行，把它们标 stale 只是谎。
//!
//! 纪律：水位读数复用 `replicate`（同一把尺子，不在这里重算）；`--apply=false` 时
//! 除 `memory_apply_status` 的幂等建表外不写任何行；错误面只带类别（`main` 折成退出码 3
//! 与一句静态话），不回显路径/uid。
//!
//! H7c2：报告再加两条**审查轴**读数——ledger 水位里全 pass 的最大 `max_decision_id` 与
//! 本地 `review_decision` 的最大 id。两者不等只在文本末尾提示差值（`review_gap=`），
//! 不改任何状态：补审还是重放是人看台账后的决定，不是这里的自动动作。

use crate::replicate;
use rc_ledger::pass_state::{self, PassKey, PassState, PassStateRow};
use rc_ledger::transcript::turn_uids_in_commit_range;
use rc_store::{Root, StoreError};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

/// 缺口区间一次最多处理的轮数。超过它也只处理前这么多，并置 `truncated=true`——
/// 截断是**读数**，不是错误：接管后可以再跑一次（记忆水位会因重蒸而前进）。
pub const GAP_TURN_LIMIT: usize = 10_000;

/// `--apply` 写进 pass 行的理由 slug。与 `pass_state` 的其它理由同族（`[a-z0-9_]+`）。
pub const MEMORY_GAP_REASON: &str = "memory_gap";

/// 一次对账的读数（`--json` 是同一组字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GapReport {
    /// 记忆快照覆盖到的转录提交号（`replicate::memory_apply_status`）。
    pub memory_commit_seq: u64,
    /// 本地转录轴的水位（`replicate::status`）。
    pub transcript_commit_seq: u64,
    /// 缺口区间内**实际处理**的轮数（截断后；`truncated` 说明还有更多）。
    pub gap_turns: usize,
    /// 区间里的轮数超过 [`GAP_TURN_LIMIT`]：只处理了前 `GAP_TURN_LIMIT` 轮。
    pub truncated: bool,
    /// 缺口内这些轮的 pass 行按状态的计数，slug 字典序。
    pub by_state: Vec<(String, usize)>,
    /// 至少有一条 `done` 行的**轮**数（与 `by_state` 的行数不是一回事）。
    pub done_turns: usize,
    /// `--apply` 时真正改成 `stale` 的 pass 行数（只读时恒 0）。
    pub marked_stale: usize,
    /// H7c2：ledger 的审查轴水位里**全 pass 的最大** `max_decision_id`（0 = 没有水位行）。
    pub ledger_review_max_decision_id: i64,
    /// H7c2：本地 `review_decision` 的最大 id（0 = 本地一条判定都没有）。
    /// 两者之差 = "审查差多少条"；本命令只提示，不改任何状态。
    pub local_review_max_decision_id: i64,
    /// S4a4：这台机器从未应用过记忆快照（中心本地库/还没拉过快照的备份）。
    /// 此时 `memory_commit_seq=0` 不是"落后"，不该把整段转录误报成缺口。
    pub not_a_replica: bool,
}

/// 命令面的失败类别。`main` 把它们折成退出码 3 与一句静态话。
#[derive(Debug)]
pub enum ReconcileError {
    /// 存储/水位读失败。
    Store(StoreError),
    /// 报告序列化失败（`--json`）。
    Encode,
}

impl From<StoreError> for ReconcileError {
    fn from(error: StoreError) -> Self {
        ReconcileError::Store(error)
    }
}

/// 算记忆/转录水位缺口；`apply=true` 时把缺口内 `done` 的 pass 行标 `stale(memory_gap)`。
///
/// 水位比较：`memory >= transcript` ⇒ 没有缺口，除两条水位读数外全 0（不改库）。
/// 缺口区间取 `(memory, transcript]`——`memory_commit_seq` 是已覆盖到的那一格。
/// `now_ms` 只进 `--apply` 的 `updated_at_ms`。
pub fn memory_gap(root: &Root, apply: bool, now_ms: i64) -> Result<GapReport, ReconcileError> {
    let status = replicate::memory_apply_status(root.ledger())?;
    let memory_commit_seq = status.commit_seq;
    let transcript_commit_seq = replicate::status(root.transcript())?.last_commit_seq;
    // H7c2：审查轴的两条读数与记忆缺口**无关**地取一次——没有缺口时也要报（接管现场
    // 最常见的就是"记忆追平了、审查还差几条"）。
    let (ledger_review_max_decision_id, local_review_max_decision_id) = review_max(root)?;
    // S4a4：`memory_replication_state` 在每个库上都会被建出来（0,'',NULL）。只有真的
    // 应用过快照的备份才谈得上"缺口"；中心本地库把 0 当缺口会误报整段转录。
    if !status.applied_any {
        return Ok(GapReport {
            memory_commit_seq,
            transcript_commit_seq,
            gap_turns: 0,
            truncated: false,
            by_state: Vec::new(),
            done_turns: 0,
            marked_stale: 0,
            ledger_review_max_decision_id,
            local_review_max_decision_id,
            not_a_replica: true,
        });
    }
    if memory_commit_seq >= transcript_commit_seq {
        return Ok(GapReport {
            memory_commit_seq,
            transcript_commit_seq,
            gap_turns: 0,
            truncated: false,
            by_state: Vec::new(),
            done_turns: 0,
            marked_stale: 0,
            ledger_review_max_decision_id,
            local_review_max_decision_id,
            not_a_replica: false,
        });
    }

    // 多问一条：`limit + 1` 有货就是截断。`GAP_TURN_LIMIT` 是 usize，加一不会溢出（它是 10_000）。
    let probe = turn_uids_in_commit_range(
        root.transcript(),
        memory_commit_seq,
        transcript_commit_seq,
        GAP_TURN_LIMIT + 1,
    )?;
    let truncated = probe.len() > GAP_TURN_LIMIT;
    let uids: Vec<String> = probe.into_iter().take(GAP_TURN_LIMIT).collect();

    let rows = pass_state::rows_for_turns(root.ledger(), &uids)?;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut done_turns: BTreeSet<&str> = BTreeSet::new();
    for row in &rows {
        *counts.entry(row.state.slug().to_string()).or_default() += 1;
        if row.state == PassState::Done {
            done_turns.insert(row.turn_uid.as_str());
        }
    }

    // 只标 `done`：其余状态没有"已写但缺失"的行（见模块头）。逐**行**标，fact 的各 family
    // 各自一条；任何一条写失败就整体报错（不做半途而废的静默）。
    let mut marked_stale = 0usize;
    if apply {
        for row in rows.iter().filter(|r| r.state == PassState::Done) {
            mark_stale(root, row, now_ms)?;
            marked_stale += 1;
        }
    }

    Ok(GapReport {
        memory_commit_seq,
        transcript_commit_seq,
        gap_turns: uids.len(),
        truncated,
        by_state: counts.into_iter().collect(),
        done_turns: done_turns.len(),
        marked_stale,
        ledger_review_max_decision_id,
        local_review_max_decision_id,
        not_a_replica: false,
    })
}

/// H7c2：审查轴的两条读数。
///
/// - ledger 侧取**全 pass 的最大** `review_watermark.max_decision_id`（水位按 pass 分行，
///   报告只有一格）；空表 ⇒ 0。
/// - 本地侧读 `review_decision` 的最大 id；空表 ⇒ 0。
///
/// 只读，不改任何状态。错误面折成 `Store`（`main` 给退出码 3 与一句静态话）。
fn review_max(root: &Root) -> Result<(i64, i64), ReconcileError> {
    let ledger = rc_ledger::review_mark::all(root.ledger())?
        .into_iter()
        .map(|w| w.max_decision_id)
        .max()
        .unwrap_or(0);
    let local = rc_memory::review::max_decision_id(root.memory()).map_err(|_| {
        ReconcileError::Store(StoreError::Sql(
            "reconcile: review max decision id unreadable".to_string(),
        ))
    })?;
    Ok((ledger, local))
}

fn mark_stale(root: &Root, row: &PassStateRow, now_ms: i64) -> Result<(), ReconcileError> {
    pass_state::mark_terminal(
        root.ledger(),
        PassKey {
            turn_uid: &row.turn_uid,
            pass_id: &row.pass_id,
            family: &row.family,
        },
        PassState::Stale,
        Some(MEMORY_GAP_REASON),
        now_ms,
    )?;
    Ok(())
}

/// 两形输出：文本一行（`by_state` 空写 `-`），`--json` 是同一组字段。
///
/// H7c2：文本在末尾加 `ledger_review_max_decision_id=`/`local_review_max_decision_id=`；
/// 两者不等时再追加一格 `review_gap=`（= ledger − local，负数就是本地比水位多）。
/// 它只是提示，不改任何状态——差多少要人看台账再决定补审还是重放。
pub fn render(report: &GapReport, json: bool) -> Result<String, ReconcileError> {
    if json {
        return serde_json::to_string(report).map_err(|_| ReconcileError::Encode);
    }
    let mut text = format!(
        "memory_seq={} transcript_seq={} gap_turns={} truncated={} done_turns={} \
         marked_stale={} by_state={} ledger_review_max_decision_id={} \
         local_review_max_decision_id={} not_a_replica={}",
        report.memory_commit_seq,
        report.transcript_commit_seq,
        report.gap_turns,
        report.truncated,
        report.done_turns,
        report.marked_stale,
        by_state_text(&report.by_state),
        report.ledger_review_max_decision_id,
        report.local_review_max_decision_id,
        report.not_a_replica,
    );
    let gap = report.ledger_review_max_decision_id - report.local_review_max_decision_id;
    if gap != 0 {
        text.push_str(&format!(" review_gap={gap}"));
    }
    Ok(text)
}

fn by_state_text(by_state: &[(String, usize)]) -> String {
    if by_state.is_empty() {
        return "-".to_string();
    }
    by_state
        .iter()
        .map(|(state, count)| format!("{state}={count}"))
        .collect::<Vec<_>>()
        .join(",")
}
