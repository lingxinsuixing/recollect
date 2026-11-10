//! G1 刀 A（设计 `2026-11-08-g1-reuse-lesson-design.md` §波次一第 1 层）：
//! **程序资格预闸**（纯程序、零模型成本）与 **archive 载体**。
//!
//! 判据（派发档语义裁定①②，照做不另发明）：
//! - 命中 = 三者合取：①主体/说话人 = agent 自锚（两槽任一命中即算——说话人槽由 C1
//!   程序盖章，无 human 证据且无轮级 speaker 时回落到会话自锚）；②出处无 human 轮参与
//!   （[`human_provenance_exists`]，与 G11 的 `anchor_role == Human` 同一入口同一把尺子）；
//!   ③谓词 ∈ 施工谓词闭集（归一化后**精确相等**，不命中不预闸——宁漏勿错，刀 B 的
//!   LLM 价值轴兜底）。
//! - **豁免优先于命中**：出处任一环有 human ∨ category ∈ (decision, promise, plan)
//!   ∨ 谓词 ∈ 决策谓词闭集（决定/拍板/裁定/要求/承诺）。
//!
//! 命中 ⇒ 镜像卡落 `card_sink(sink='archive')`：**原文全留、照常进库、不改 verdict、
//! 不动 vote**，只是召回主循环缺省不取（消费在 `rc_search::rank::rank_counted` 单一读口）。
//!
//! ## 载体：canonical 正路（G1 刀 B 第 0 件起）
//!
//! DDL 的唯一家是 [`rc_store::ddl::CARD_SINK_SQL`]，它进 `MEMORY_SQL` 重放列 ⇒ **开库即
//! 存在**；表名进 `rc_memory::schema::MEMORY_TABLES` 与归属表 ⇒ hub 的 rows 吸收、backup
//! 摘要与 restore 整替都带着章走。`MEMORY_VERSION` 不 bump（`IF NOT EXISTS` 增量补齐）。
//!
//! 为什么必须升清单（刀 A 的模块私有侧表先例在这里不成立）：`hub::restore::overwrite_memory`
//! 按 `MEMORY_TABLES` **逐表整替**，清单外的表在恢复后回到空库形状。程序章
//! （`reason='program_gate'`）还能靠回填同一把尺子再生，刀 B 的 LLM 价值轴章
//! （`reason='value_axis'`）**不可再生**——不随备份恢复就永久丢了。`review_verdict` 之所以
//! 仍能例外，是因为它是可重跑的证据附件，不是消费面状态。
//!
//! 两处兼容保留：
//! - 写路径的 [`ensure`] 幂等补表留作**双保险**（重放同一根 `CARD_SINK_SQL`，不另写一份
//!   DDL）：刀 A 的测量夹具会先 `DROP TABLE card_sink` 再回填，补表让那条路径不用改。
//! - 读路径 [`archived_card_ids`] **缺表 = 空集**：刀 A 之前开的只读旧副本没有这张表
//!   （只读面不跑 `migrate_memory`），仍要能跑而不是报错。
//!
//! `reason` 列**不加 CHECK**：两族章（`program_gate` / `value_axis`）同表共存，
//! 加 CHECK 就得重建表（SQLite 改不了 CHECK）。`sink` 列今天只有 `archive` 一个值，
//! CHECK 钉死它。盖章一律 **先写者赢**（[`mark_archive`] 的 `INSERT OR IGNORE`）：
//! 已有程序章的卡不会被价值轴覆盖，反之亦然。

use crate::keys::AnchorRole;
use crate::MemError;
use rc_store::MemoryHandle;
use std::collections::HashSet;

/// 程序预闸盖章的 `reason` 值（回填与写点共用，报告与测量按它分族）。
pub const PROGRAM_GATE_REASON: &str = "program_gate";

/// LLM 价值轴盖章的 `reason` 值（G1 刀 B，设计 §波次一第 2 层）。与程序章**同表同待遇**
/// （`sink='archive'`，召回主循环缺省不取），先写者赢：已有程序章的卡不被价值轴覆盖。
/// 值域的家在这里，审核侧的落章点与测量都引用它，不各抄一份字面量。
pub const VALUE_AXIS_REASON: &str = "value_axis";

/// 豁免类别闭集（派发档语义裁定②）：即使谓词撞施工词，这三类照旧 main。
pub const EXEMPT_CATEGORIES: [&str; 3] = ["decision", "promise", "plan"];

/// 决策谓词闭集（派发档语义裁定②逐字五词）。现网的 decide/plan/promise 谓词行
/// 已由 category 豁免覆盖，这一格守的是"类别没标对、谓词标对了"的行。
pub const DECISION_PREDICATES: [&str; 5] = ["决定", "拍板", "裁定", "要求", "承诺"];

/// 施工谓词闭集（**全词表**，归一化后精确相等）。
///
/// 定词依据（2026-11-08 现网库只读取证，`fact WHERE mode='stated'` 共 160 个不同谓词）：
/// - 现网分布里的**机械执行/核验/状态查询**动词全族收录（check_* 22 词、verify_* 24 词、
///   dispatch_* 4 词、report_* 的状态报告 6 词、complete_* 的流水完成 4 词、
///   confirm_* 的状态确认 5 词，以及 run/rerun/deploy/rebuild/start/stop/attach 等 28 词）；
/// - 计划例词补齐中英基础动词 14 词（部署/重启/监听/提交/构建/运行/测试 + 英文对应）。
///
/// **宁漏勿错的排除项**（不收录，留给刀 B 的 LLM 轴）：里程碑（land/land_w31/
/// land_identity_model/complete_t1/complete_w32_stopgap_wave/accept_t2）、发现与调查
/// （find_* 8 词、discover_correction、observe_* 3 词、notice_* 2 词、judge_model_unavailable、
/// investigate_caller_and_foreground、trace_wall_link、review_search_results、
/// review_test_memories、summarize_probe_results、deep_dive_codegraph）、失败与修复状态
/// （process_died、fix_effective、fix_take_effect、report_assistant_failure、
/// report_hook_timeout、report_self_check_failure、report_state_error）、设计/问题确认
/// （confirm_design/change/chain/issue_4/p5_closure/p5_handoff）、配置与迁移一次性动作
/// （change_*、disable_watermark_gate、re-enable、first_trigger_after_reenable、
/// empty_stash_list、end_previous_instance、backfill_speaker_anchor、wire_derive、
/// collect_data、clear_alias_ambiguity_warnings、register_comment_inconsistency、
/// abandon_batch_8、attempt_s4u、decide_manual_maintenance）。
/// 全词表与逐词取舍见报告 `design/2026-11-08-g1a-program-gate-readings.md`。
pub const CONSTRUCTION_PREDICATES: [&str; 117] = [
    // ── 现网分布：执行/启动/停止族（10）──
    "attach_listener", "stop_monitor", "run_tests", "rerun_tests", "run_pipeline",
    "run_distill_sample", "deploy_release", "rebuild_fts", "start_drain", "start_shape_probe",
    // ── 现网分布：check_* 状态检查族（22）──
    "check_api_access", "check_causal_cards", "check_db_stats", "check_drain_status",
    "check_enable_commands", "check_endpoint", "check_episode_calls", "check_episode_output",
    "check_extraction", "check_fact_decision_logs", "check_fact_link_edges", "check_head_status",
    "check_log", "check_pipeline_status", "check_processes", "check_relation_table",
    "check_residual", "check_script_location", "check_server_stats", "check_state_assertion",
    "check_subtitle_status", "check_t2_existence",
    // ── 现网分布：verify_* 核验族（24）──
    "verify_a7_speaker", "verify_a8_v2v3", "verify_a9_report", "verify_bilibili_login",
    "verify_code_logic", "verify_commit_existence", "verify_consistency", "verify_existence",
    "verify_file_set", "verify_fix", "verify_four_questions", "verify_handover",
    "verify_issues", "verify_plan_and_head", "verify_readonly", "verify_self_card_gate",
    "verify_session_issues", "verify_speaker_gap", "verify_stuck", "verify_task",
    "verify_task_status", "verify_v2_v3_gap", "verify_w28_backup", "verify_write_points",
    // ── 现网分布：report_* 状态报告族（6，失败报告四词不收）──
    "report_case_results", "report_crlf_only", "report_diff_status", "report_dirty_files",
    "report_mtimes", "report_probe_status",
    // ── 现网分布：complete_* 流水完成族（4，里程碑两词不收）──
    "complete_background_run", "complete_forensics", "complete_preflight", "complete_qr_login_task",
    // ── 现网分布：confirm_* 状态确认族（5，设计/问题确认六词不收）──
    "confirm_api_quota", "confirm_binary_exists", "confirm_quota", "confirm_residual_row",
    "confirm_running_no_reentry",
    // ── 现网分布：dispatch_* 派发族（4）──
    "dispatch_reviewer", "dispatch_subagents", "dispatch_tasks", "dispatch_w31",
    // ── 现网分布：其余机械动作（28）──
    "test_schtasks", "test_controller", "smoke_test_night_archive", "sync_script",
    "modify_task_action", "kill_tail_exe", "prepare_worktree", "perform_preflight",
    "request_test", "submit_manual_trigger", "trigger_new_round", "poll_login_status",
    "generate_qrcode", "save_login_credential", "install_tasks", "import_corpus",
    "ingest_conversation", "refresh_backup", "update_memory", "read_memory",
    "read_session", "read_round3_receipt", "list_models", "query_codegraph",
    "fire_requests", "write_docs", "write_report", "record_dispatch",
    // ── 计划例词：中英基础动词（14）──
    "部署", "重启", "监听", "提交", "构建", "运行", "测试",
    "deploy", "restart", "monitor", "commit", "build", "run", "test",
];

/// 归一化：trim + ASCII lowercase（与 `check_alias`/`is_placeholder_mechanism` 同一把尺子）。
/// 中文不受 lowercase 影响；闭集里的英文词全小写 ⇒ 大小写混写归一后精确相等。
fn normalize(s: &str) -> String {
    s.trim().to_ascii_lowercase()
}

/// 预闸判定的全部输入（写点与回填共用这一个形状，不各拼一份）。
///
/// `self_anchor_id` 是 `session_agent_anchor_id` 的解析结果：锚不在库 ⇒ `None` ⇒
/// 条件①无法成立 ⇒ 不命中（不猜）。
pub struct GateInput<'a> {
    pub subject_anchor_id: &'a str,
    pub speaker_anchor_id: Option<&'a str>,
    pub self_anchor_id: Option<&'a str>,
    pub human_in_provenance: bool,
    pub category: &'a str,
    pub predicate: &'a str,
}

/// 程序资格预闸的**唯一判定函数**：写点（`writers_fact`）与回填（`rc_distill::value_backfill`）
/// 共用这一处——同一把尺子，两处不许各写一遍。豁免优先于命中。
pub fn program_gate_hit(input: &GateInput<'_>) -> bool {
    // 豁免①：出处任一环有 human（G11 同一口径）。
    if input.human_in_provenance {
        return false;
    }
    // 豁免②：category 三值。
    let category = normalize(input.category);
    if EXEMPT_CATEGORIES.contains(&category.as_str()) {
        return false;
    }
    let predicate = normalize(input.predicate);
    // 豁免③：决策谓词五词。
    if DECISION_PREDICATES.contains(&predicate.as_str()) {
        return false;
    }
    // 合取①：主体/说话人任一槽 = 会话自锚。
    let Some(self_id) = input.self_anchor_id else {
        return false;
    };
    let self_hit = input.subject_anchor_id == self_id
        || input.speaker_anchor_id == Some(self_id);
    // 合取②（human 已在豁免里判过）+ 合取③：谓词精确命中闭集。
    self_hit && CONSTRUCTION_PREDICATES.contains(&predicate.as_str())
}

fn table_exists(m: &MemoryHandle) -> Result<bool, MemError> {
    let n: i64 = m.conn().query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='card_sink'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 幂等地补表（**双保险**）。正路是 `MEMORY_SQL` 开库即建；这里重放的是同一根
/// [`rc_store::ddl::CARD_SINK_SQL`]，不是第二份 DDL。只在写路径调（只读副本没有写入面；
/// 读路径走缺表=空集）。留着它的理由：先 `DROP TABLE` 再盖章的夹具/旧库路径不必改。
fn ensure(conn: &rusqlite::Connection) -> Result<(), MemError> {
    conn.execute_batch(rc_store::ddl::CARD_SINK_SQL)?;
    Ok(())
}

/// 给一张卡盖 `sink='archive'` 章。**先写者赢**：`INSERT OR IGNORE` ⇒ 重放/二次回填
/// 返回 0，不覆盖既有章（ reason/at_ms 都不动）。返回本次真插入的行数（0 或 1）。
///
/// 调用方负责事务边界：写点在落卡事务内盖（与卡同生死），回填自己开一个事务。
pub fn mark_archive(
    conn: &rusqlite::Connection,
    card_id: &str,
    reason: &str,
    at_ms: i64,
    norm_version: i64,
) -> Result<usize, MemError> {
    ensure(conn)?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO card_sink(card_id, sink, reason, at_ms, norm_version)
         VALUES(?1,'archive',?2,?3,?4)",
        rusqlite::params![card_id, reason, at_ms, norm_version],
    )?;
    Ok(n)
}

/// 读口：全库被归档的卡 id 集。**缺表 = 空集**（老库自愈的读侧那一半），
/// 只读副本也能跑。`rank_counted` 每次调用装载一次（与 GateIndex 同一条代价账）。
pub fn archived_card_ids(m: &MemoryHandle) -> Result<HashSet<String>, MemError> {
    if !table_exists(m)? {
        return Ok(HashSet::new());
    }
    let mut st = m.conn().prepare("SELECT card_id FROM card_sink WHERE sink='archive'")?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = HashSet::new();
    for id in rows {
        out.insert(id?);
    }
    Ok(out)
}

/// 被归档的 **fact 行** id 集：镜像卡的 `pointers_json` 里有 `fact:<id>` 且该卡盖了
/// `sink='archive'` 章的 fact 都算（同一 fact 只要有一张镜像卡被归档就按归档处理——
/// 写点与回填都在同一事务里把该 fact 的全部镜像卡一次盖完，混合态只可能来自半截失败，
/// 那时按"已归档"更保守）。视图面用它做归档过滤；**显式寻址（handles/card/expand）
/// 不过这里**，原文全留、指认即给。
///
/// 缺表（旧库/只读副本）= 空集，与 [`archived_card_ids`] 同一条兼容。
pub fn archived_fact_ids(m: &MemoryHandle) -> Result<HashSet<String>, MemError> {
    if !table_exists(m)? {
        return Ok(HashSet::new());
    }
    let mut st = m.conn().prepare(
        "SELECT DISTINCT substr(p.value, 6)
           FROM card c, json_each(c.pointers_json) p
          WHERE c.id IN (SELECT card_id FROM card_sink WHERE sink='archive')
            AND p.value LIKE 'fact:%'",
    )?;
    let rows = st.query_map([], |r| r.get::<_, String>(0))?;
    let mut out = HashSet::new();
    for id in rows {
        out.insert(id?);
    }
    Ok(out)
}

/// 出处里有没有 human 轮（`anchor_role='human'`，G11 `strong_anchor` 同一把尺子）。
/// 写点查卡（镜像出处与 fact 逐条同形）、回调查 fact——同一个入口，不各写一份 EXISTS。
pub fn human_provenance_exists(
    conn: &rusqlite::Connection,
    mem_table: &str,
    mem_id: &str,
) -> Result<bool, MemError> {
    let n: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM provenance
                        WHERE mem_table=?1 AND mem_id=?2 AND anchor_role=?3)",
        rusqlite::params![mem_table, mem_id, AnchorRole::Human.as_str()],
        |r| r.get(0),
    )?;
    Ok(n != 0)
}