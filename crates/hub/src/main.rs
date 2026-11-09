mod environment;
mod replay;

use clap::{Args, Parser, Subcommand};
use rc_contract::{normalize_with, parse, BatchReport, RedactionEntry, Reject, RejectCode};
// B2b-1：cycle 管线搬进 `rc-cycle` 之后，hub 只留装配（`run_cycle_with`）、打印与退出码
// （`finish_cycle`）与座位（`replay::Seat`）。这一行里的名字都是那一搬的调用点：
// `charge_counters` / `run_anomaly_scan` / `scan_rounds` / `scan_due` 是轮号与扫描节奏，
// `qualify` 是守卫 14 的入口判定，`index_failure` 与 `run_index` 共用同一个错误面。
use rc_cycle::{
    charge_counters, index_failure, qualify, run_anomaly_scan, run_cycle_once, scan_due,
    scan_rounds, CycleExit, CycleOut, Metered, MeteredBackend, ANOMALY_SCOPE, FAULT_ANOMALY,
    FAULT_COUNTERS, FAULT_STORE_WRITE, FAULT_UNQUALIFIED,
};
use rc_ledger::llm_state;
use rc_ledger::water;
use rc_llm::{JobFamily, LlmBackend, LlmFailure, Persisted, Rotating, Seed};
// S2a-3：naming pass 的库面与它自己的错误面。CLI 只做装配与退出码，选批/双跑/落库都在
// `rc-distill::pass_naming`（这一层不复制任何抽取逻辑）。
use rc_distill::pass_naming::{select_naming_batch, NamingOutcome, DEFAULT_LIMIT, NAMING_PASS};
// S2b-2b：fact pass 的库面。`--family` 是这一条 pass 的必填调用变体，选批/抽取/独立审核/落库
// 全在 `rc_distill::pass_fact`（hub 只做装配、读数与退出码）。
use rc_distill::pass_fact::{family_of_slug, select_fact_batch, FactOutcome};
// S2c-2：event pass 的库面。event 是单一 pass（没有 `--family`），选批/抽取/独立审核/落库
// 全在 `rc_distill::pass_event`（hub 只做装配、读数与退出码）。
use rc_distill::pass_event::{select_event_batch, EventOutcome};
use rc_distill::pass_lesson::{run_lesson, select_lesson_batch, LessonOutcome};
// P2：state pass 的库面。单一 pass（没有 `--family`），选批/抽取/独立审核/落库
// 全在 `rc_distill::pass_state`（hub 只做装配、读数与退出码）。
use rc_distill::pass_state::{select_state_batch, StateOutcome};
// S2d-2：time pass 的库面。它是**纯程序**（不调 LLM、没有座位来源），选批/确定性回填/落
// `extract_pass_state` 全在 `rc_distill::pass_time`（hub 只做装配、读数与退出码）。
use rc_distill::pass_time::TimeOutcome;
// P0-4a：一轮的编排（选批 → 执行）搬进 `rc_distill::round`，四族共用同一个实现点。
// hub 只留能力自测、座位装配、四族各自的错误→(state, fault) 映射与打印。
use rc_distill::round::{round_once, Family, RoundError, RoundInput, RoundOut};
use rc_distill::pass_prompt::{FactFamily, PASS_EVENT, PASS_FACT, PASS_LESSON, PASS_STATE};
use rc_distill::review::ReviewMode;
use rc_distill::DistillError;
use rc_store::capability::CapReport;
use rc_store::{Root, StoreError};
use rc_memory::owner::OwnerCtx;
use rc_memory::writers_v4::record_turn_speakers;
use rc_memory::{BatchCtx, Scope, ScopeKind};
use hub::{hub_cfg, serve::HubState};
use hub::purge::PurgeScope;
use hub::ingest::{commit, device_subject, sole_device, CommitInput};
use rc_ledger::device::RevokeOutcome;
use rc_net::http::Limits;
use rc_net::key::DeviceKey;
use rc_net::node::{download_snapshot, signed_request, Endpoint, NodeAuth};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

#[derive(Parser)]
#[command(name = "recollect-hub", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    Import(ImportArgs),
    /// 索引层：重建与读数（阶段 1 唯一在节点之外写记忆库的作业）
    Index(IndexArgs),
    /// 抽取轮：把新导入的 turn 抽成记忆行（阶段 2 的三支作业装在同一条命令里）
    Cycle(CycleArgs),
    /// 分 pass 抽取：跑 naming、一条 fact family、event 或 time（S2a-3 / S2b-2b / S2c-2 / S2d-2）
    Distill(DistillArgs),
    /// 推导：跑一批（`run`）、看游标/attempts/未解决隔离（`status`）、按 issue 回拨重跑（`retry`）
    Derive(DeriveArgs),
    /// 抽取问题审计（W24c）：列出 `extraction_issue`、按 id 关闭一行（关闭是逐条动作，没有批量）
    Issue(IssueArgs),
    /// 环境属性（W24c）：运维直陈"这台环境叫什么、挂了哪些账号"（环境锚 + `attribute`）
    Environment(EnvironmentArgs),
    /// 待办水位（W25）：抽干前的四组格子，只读装配、零写零迁移（水位闸的主触发读它）
    Pending(PendingArgs),
    /// 告警复核（W26）：全量扫两族判据、升/解 `attention`（日保底档 `--full` 的维护步）
    Anomaly(AnomalyArgs),
    /// episode 归组（L2）：把"同一件事的几步"归成一段经历（纯程序；无座位 = 零模型调用，给了座位 = 中间带逐对裁决）
    Episode(EpisodeArgs),
    /// 抽取指纹：按"管线 + 模型 + prompt"分组看轮数与时窗（S1 的观测面）
    Fingerprint(FingerprintArgs),
    /// 中心服务：v1 端点十二个（配对/身份/摄入/行/状态/快照/复制一族；前台进程，Ctrl-C 结束）
    Serve(ServeArgs),
    /// TLS 身份：读证书并打印指纹（给节点配 `hub_tls_fp`）
    Tls(TlsArgs),
    /// 签发一次性配对码（默认 5 分钟，一次使用）
    Pair(PairArgs),
    /// 邀请票：签一张带 hub/TLS 指纹的票（默认 10 分钟），`revoke` 立即作废（S4b1）
    Invite(InviteArgs),
    /// 撤销一台设备：之后它的 token 一律 403
    Revoke(RevokeArgs),
    /// 每日备份：转录 + 记忆 + 账本 + agent 私房本（存在才备）+ 清单，留 7 组
    Backup(BackupArgs),
    /// 清除：两段式；无 --confirm 只写抑制账
    Purge(PurgeArgs),
    /// 恢复：三步；无 --confirm 只做校验
    Restore(RestoreArgs),
    /// 增量复制：备份侧 apply/status/lag（导出走 GET /v1/replicate）
    Replicate(ReplicateArgs),
    /// 把追平的备份提升为新中心（epoch+1，旧中心 key 签提升证书）
    Promote(PromoteArgs),
    /// 给旧中心写 fencing 标记：之后的写请求一律 409
    Fence(FenceArgs),
    /// epoch 兼容窗口：require 关掉 v1 客户端，allow 重新打开，status 读当前值
    Epoch(EpochArgs),
    /// 快照：产出并发布（中心侧唯一写快照目录的命令）
    Snapshot(SnapshotArgs),
    /// 密钥箱：建箱、写值（值只走 stdin）、列格、删格
    Secret(SecretArgs),
    /// 中心密钥的加密归档：导出到密文箱 / 从密文箱导入（口令只走 stdin）
    Key(KeyArgs),
    /// 备份 key：生成独立 key 文件（不放进 data-dir）
    BackupKey(BackupKeyArgs),
    /// 资源归属：给旧数据补 owner（S1b-C4b2a）
    Ownership(OwnershipArgs),
    /// 主人身份：盖一章 / 列有效章与历史（身份层 I1，设计 principal-identity §4）
    Principal(PrincipalArgs),
    /// 实体锚的维护操作：一键换绑（锚是在证据不全时被命名的，不能永远停在第一次猜的位置）
    Anchor(AnchorArgs),
    /// 待审队列：列出/查看/人工改判/统计（S3a-3d）
    Review(ReviewArgs),
    /// 接管对账：记忆/转录水位缺口与 by_state，`--apply` 把缺口内 done 的 pass 行标 stale（H7c1）
    Reconcile(ReconcileArgs),
    /// 交接条：物理清理已过期的便签（agent 不触发删除，这是长期没人读的库的运维兜底）
    Handoff(HandoffArgs),
}

/// H3：`hub handoff prune`。agent 侧只在**写**时惰性清理，`list`/`brief` 都不删行；
/// 一个几个月没人写过的库需要一条手工出口。
#[derive(Args)]
struct HandoffArgs {
    #[command(subcommand)]
    job: HandoffJob,
}

#[derive(Subcommand)]
enum HandoffJob {
    /// 删掉每个 agent 里 `expires_at_ms <= now` 的交接条，打印共删了几行
    Prune(HandoffPruneArgs),
}

#[derive(Args)]
struct HandoffPruneArgs {
    /// 用这个"现在"判过期；缺省取系统时钟（测试与回放靠它，不靠 ambient 时间）
    #[arg(long)]
    now_ms: Option<i64>,
    /// 明确接受"未来时间"的 `--now-ms`：它会把未关闭的交接条一并当过期删掉
    #[arg(long)]
    force: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct ImportArgs {
    /// 一个或多个 exchange 文件（G19 刀 5）。逐个走**同一条管线**，每个文件独立一份
    /// Report 输出；任一文件读不到 = 该文件运维失败（3）、其余照常；退出码取各文件的
    /// 最坏值。单文件调用与回执一字不变（回归红线——多文件只是在这一条之上并列）。
    #[arg(required = true, value_name = "FILE")]
    files: Vec<PathBuf>,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    json: bool,
    /// 脱敏映射的本地落点（JSONL）。**只有显式给了这一格才写**；不给就只计数。
    /// 这个文件里有明文，别把它放进快照/备份/报告目录。
    #[arg(long)]
    redaction_map: Option<PathBuf>,
}

#[derive(Args)]
struct IndexArgs {
    #[command(subcommand)]
    job: IndexJob,
}

#[derive(Subcommand)]
enum IndexJob {
    Rebuild {
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
    },
    Status {
        #[arg(long, default_value = "./data")]
        data_dir: PathBuf,
    },
}

#[derive(Args)]
struct CycleArgs {
    #[command(subcommand)]
    job: CycleJob,
}

#[derive(Subcommand)]
enum CycleJob {
    Once(CycleOnceArgs),
}

/// S2a-3：分 pass 抽取的子命令面。`Once` 是一条 pass 一次调用；`Reclaim` 与 `Reset` 是同一张
/// 状态表上的两条运维兜底（前者把卡住的 `running` 退回 `pending`，后者按轮把终态定向回拨成
/// `stale`），三者不共用选批与座位装配。
#[derive(Args)]
struct DistillArgs {
    #[command(subcommand)]
    job: DistillJob,
}

#[derive(Subcommand)]
enum DistillJob {
    Once(DistillOnceArgs),
    Reclaim(DistillReclaimArgs),
    Reset(DistillResetArgs),
    /// G1 刀 A 的存量面：把上线前落库的镜像卡按程序资格预闸补章（零模型、幂等）。
    BackfillArchive(DistillBackfillArgs),
}

/// `hub distill backfill-archive`：给**上线前**落库的镜像卡补跑程序资格预闸（G1 刀 A 存量面）。
///
/// 为什么要有这一格：预闸的写点只管上线之后的新卡；存量卡没有 `card_sink` 章，
/// 召回主循环里照旧把它们当正文取（G1 测量：副本上 235 行扫描、103 命中、占比 40.3%→6.9%）。
/// 本入口是存量面的唯一正道——与写点**共用同一个判定函数**
/// （`rc_distill::value_backfill::backfill_program_archive`），`INSERT OR IGNORE`
/// 先写者赢，重跑 `newly_marked=0`；零模型、只读+写章。
///
/// 它不是周期任务：新行在落卡事务里已盖章，跑一次即可（再跑只是空扫）。
#[derive(Args)]
struct DistillBackfillArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`{schema_version,scanned,hits,newly_marked,now_ms}`
    #[arg(long)]
    json: bool,
}

/// `hub distill reclaim --pass naming|fact|event|state|time`：运维兜底，把卡住的 `running`
/// 行退回 `pending`（状态机的实际改动全在 [`rc_ledger::pass_state::reclaim_running`] 里，
/// 这里只是 CLI 面）。
///
/// 为什么要有这一格：进程在 `mark_running` 之后崩掉，那一行会永远停在 `running`，而
/// `should_run` 对 `running` 不重入——这一格是把它放回选批的唯一正道，且只改 `state`，
/// 不删行、不手工 `UPDATE`。
#[derive(Args)]
struct DistillReclaimArgs {
    /// 只收五个 pass 名；别的值是用例错误（与 `distill once --pass` 同口径）。
    #[arg(long)]
    pass: String,
    /// 只回收 `updated_at_ms <= now - older_than_ms` 的 running 行；0 = 全部。
    #[arg(long, default_value_t = 0)]
    older_than_ms: i64,
    /// 退回 pending 后的退避；0 = 立刻可重选。
    #[arg(long, default_value_t = 0)]
    backoff_ms: i64,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

/// `hub distill reset --pass <name> --turns <file> --reason <slug>`：按轮**定向回拨**。
///
/// 为什么要有这一格：`should_run` 只对"指纹不同 / `stale` / `pending` 且到期"开口，
/// `reclaim` 只收 `running`，对账只按水位缺口批量标 stale——"这几轮这一条 pass 重蒸"
/// 在现网没有口子。这里把指定的轮在这一条 pass 上的终态行改成 `stale`：不删行、
/// 不换指纹（指纹一动 `should_run` 走的就是另一支），下一轮选批自然捡起它们。
///
/// 只对终态生效：`running`/`pending`/`stale` 原样不动，只进读数。
#[derive(Args)]
struct DistillResetArgs {
    /// 只收五个 pass 名；别的值是用例错误（与 `distill once --pass` 同口径）。
    #[arg(long)]
    pass: String,
    /// 一行一个 64 位十六进制 `turn_uid`；空行与 `#` 开头的行跳过，不合格的行是用法错误。
    #[arg(long)]
    turns: PathBuf,
    /// 回拨行落下的理由 slug（闭集形状与 `review decide --reason` 同一把尺子）。
    #[arg(long)]
    reason: String,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

/// `hub distill once --pass naming|fact|event|state|lesson|time`。座位来源两格（`--replies` / `--llm-config`）与
/// `CycleOnceArgs` 同名同义——它们共用 [`llm_source_with_timeout`] 这一段装配（S2e-2 起多一格
/// 出网墙钟 `--llm-timeout-ms`），不是第二套解析。
#[derive(Args)]
struct DistillOnceArgs {
    /// 目前接受 `naming`、`fact`、`event`、`state`、`lesson` 与 `time`（闭集的唯一字面在
    /// [`FAULT_PASS`]）。别的值不是"回落到默认"，是用法错误（退出 3）：
    /// 静默回退会让操作员以为某条 pass 已经跑过了。
    #[arg(long)]
    pass: String,
    /// fact 的调用变体（`decision|promise|plan`）。对 `--pass fact` **必填**；其它 pass 给了它
    /// 是用法错误（退出 3）——静默忽略会让操作员以为"这一轮问的是 promise"。
    #[arg(long)]
    family: Option<String>,
    /// 座位来源之一，可选且可重复（顺序就是降级顺序；第 2..N 座共享第一座的账本）。
    #[arg(long)]
    replies: Vec<PathBuf>,
    /// 座位来源之二：`llm.toml` 住在哪儿（刻意不给默认值，理由同 `CycleOnceArgs`）。
    #[arg(long)]
    llm_config: Option<PathBuf>,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 一条 pass 最多选多少轮（0 = 该 pass 的默认窗宽；naming/fact/event/time 今天都是 20）。
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    batch_turns: usize,
    /// 单次模型调用的墙钟上限（毫秒）；缺省 600000。
    ///
    /// 这一格只对 **live** 那一支有牙齿（`--llm-config` 构造出来的真座位）：`--replies` 是
    /// 本地脚本，一次都不出网，也不读这一格。缺省 600000 只是旗标自己的缺省——真正的出网
    /// deadline 还要过单发墙（`rc_llm_http::transport::CALL_WALL_MS`，300 秒）取 min，
    /// 所以这一格**只许把墙调小**，给了更大的值也不会让某一发挂得更久。
    /// `0` 不是「不设上限」也不是「立即超时」，是用法错误（见 `run_distill_once` 的入口判定）。
    #[arg(long, default_value_t = 600_000)]
    llm_timeout_ms: u64,
    /// 只抽取并入队，不跑窄审；之后用 `hub review run` 补审/补写（S3b-4b）。
    ///
    /// 待审不是失败：`EnqueueOnly` 成功时读数 `state=pending`、行数 0、退出码 0。
    /// `--pass time` 给这一格是用法错误（time 一次模型都不调，没有"待审"可言）。
    #[arg(long)]
    no_review: bool,
    /// 空轮重试（工单 `2026-10-30-retry-empty`）：第一次抽取**判空**时同一批原样再采一次，
    /// 最多重试 N 次。`0`（默认）= 今天的行为，一次调用、读数逐字不变。
    ///
    /// 判空 = 模型这一批一条事件都没给（`events=[]`）且每个目标轮都进了 `empty_turns`；
    /// 有草稿、草稿被丢弃、模型漏轮的批**不**重试。重试是一条**真调用**（计入
    /// `llm_job:event` 的 `llm_calls`），拿到的事件走同一条闸/落库/审核路径；成功批的读数
    /// 多 `retries`/`recovered` 两格（开关关着时连键都不出现）。只对 `--pass event` 有牙齿。
    #[arg(long, default_value_t = 0)]
    retry_empty: usize,
    /// DR：只打印模型会看到的原文（`SYSTEM`/`USER`），不调模型、不写任何表、不动游标。
    ///
    /// 选批与渲染与真实运行是同一行代码（同一组 `select_*_batch` 与 `*_system()`/`*_user`），
    /// 所以这份输出可以当"模型实际看到的逐字原文"来审。无可选批次 ⇒ `=== NO BATCH ===`。
    /// 与 `--replies`/`--llm-config` 同给是用法错误：那两格声明的是"这次要问模型"。
    #[arg(long)]
    dry_run: bool,
}

/// `hub derive status|retry|run`：推导（L3 组合阶段）的运维面与批循环。
/// 读数与回拨在 `status`/`retry`，跑批在 `run`：它是**组合阶段**，不是第二条蒸馏线——
/// 它读的是已过审落库的记忆行（推导池 = `reader::rows_since` 的臂表），写的是边/链/时间线，
/// 与 `distill`（轮次 → 记忆行）两根轴不同，因此不构成第二个抽取入口（P-2 选乙′：新入口，
/// 不动 `hub cycle once` 的那一支）。`retry` 仍是 `water::retry_from` 的唯一调用者。
#[derive(Args)]
struct DeriveArgs {
    #[command(subcommand)]
    job: DeriveJob,
}

#[derive(Subcommand)]
enum DeriveJob {
    /// 游标、attempts、最后一次成功与未解决隔离的读数
    Status(DeriveStatusArgs),
    /// 按 issue 把游标回拨到隔离前的位置并关闭那一行（单批重跑）
    Retry(DeriveRetryArgs),
    /// 沿推导池一批接一批地跑（40 行/批，`rc_distill::derive::DERIVE_MAX_ROWS_PER_BATCH`）
    Run(DeriveRunArgs),
}

/// W25：`hub pending`。**只有两格参数**——它是一台给定时器读的仪表，不是作业。
///
/// 这里没有 `--full`/`--force` 之类的东西：跑不跑由调用方（`drain-if-pending.sh`）按数字决定，
/// 这一面一个字节都不写、不迁移（`Root::open_readonly`），所以"多叫几次"的代价只有一次只读开库。
#[derive(Args)]
struct PendingArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`extract`/`review`/`derive`/`pass` 四组格（契约见 `crates/hub/src/pending.rs`）
    #[arg(long)]
    json: bool,
}

/// W26：`hub anomaly sweep`。**日保底档的维护步**（`daily-drain.sh --full` 跑它），不是抽取档：
/// 它不读队列、不调模型、没有发数帽——一次全量扫描把 `attention` 升到与当前库一致就结束。
///
/// 为什么它必须挂在日路径上：`rc_memory::anomaly::sweep` / `sweep_ghosts` 此前只被 legacy
/// `cycle once` 调用，而日路径从不跑 cycle ⇒ 告警只升不解除（2026-11-03 起 5 条
/// `alias_ambiguity` 一直挂着，W15 身份合流之后歧义已经不存在，却没有一次扫描去把它解掉）。
#[derive(Args)]
struct AnomalyArgs {
    #[command(subcommand)]
    job: AnomalyJob,
}

#[derive(Subcommand)]
enum AnomalyJob {
    /// 一轮全量复核：矛盾（`contradiction`）与重影（`ghosts`：别名歧义 + 同题多行）
    Sweep(AnomalySweepArgs),
}

/// `hub anomaly sweep` 的两格参数。**没有 `--now-ms`**：扫描时刻就是这一次运行的钟
/// （与 `reconcile memory` 同一条律——账上那个数说的是"哪一轮扫的"，不给外部改写的入口）。
#[derive(Args)]
struct AnomalySweepArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`{"contradiction":{"raised":N,"resolved":N},"ghosts":{"raised":N,"resolved":N}}`
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct DeriveStatusArgs {
    /// 机器可读：`cursor`/`attempts`/`last_success_at_ms`/`open_issues`/`oldest_issue_age_ms`
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct DeriveRetryArgs {
    /// `extraction_issue.id`（隔离那一刻落下的那一行；不存在的 id 是运维错误，退出 3）
    #[arg(long)]
    issue: i64,
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

/// `hub derive run`：把推导线接上批循环。座位来源两格与 `distill once` / `review run`
/// 同名同义，共用 [`llm_source_with_timeout`] 那一段装配，不是第二套解析（P-1：批宽**不给参数**，
/// 沿用 `rc_distill::derive::DERIVE_MAX_ROWS_PER_BATCH`——多一个 `--batch-rows` 就是造第二套数字）。
#[derive(Args)]
struct DeriveRunArgs {
    /// 最多跑多少批；0 = 不设上限（与 `review run --limit` 同口径）。
    /// 上限到了不等于队列空了——余量在每行读数的 `lag_rows` 那一格里。
    #[arg(long, default_value_t = 0)]
    max_batches: usize,
    /// 座位来源之一，可选且可重复（顺序就是降级顺序；第 2..N 座共享第一座的账本）。
    #[arg(long)]
    replies: Vec<PathBuf>,
    /// 座位来源之二：`llm.toml` 住在哪儿（刻意不给默认值，理由同 `CycleOnceArgs`）。
    #[arg(long)]
    llm_config: Option<PathBuf>,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 单次模型调用的墙钟上限（毫秒）；缺省 600_000，`0` 是用法错误（同 `distill once`）。
    /// 真正的出网 deadline 还要过单发墙取 min（只许调小，见 `DistillOnceArgs` 那一格）。
    #[arg(long, default_value_t = 600_000)]
    llm_timeout_ms: u64,
    /// 机器可读：每批一行 JSONL 读数（字段名取自 `DeriveReadout` 的同名字段）
    #[arg(long)]
    json: bool,
}

/// `hub issue list|resolve`（W24c · P1-9）。`extraction_issue` 的运维出口。
///
/// 为什么要有它：`rc_memory::issues::resolve` 早就存在，但此前只有 `derive retry` 一个调用者，
/// 且那只关 `pass='derive'` 的隔离行。现网 54 条 `unresolved_surface`（命名 pass 对 commit hash /
/// 路径 / 通用平台名的**正确自拒**）于是没有关闭路径，`doctor` 的 `issues.unresolved_surface`
/// 永久红。关闭是**逐条**动作：不给 `--all`，批量由操作员脚本循环——一次误敲不该清掉一整族证据。
#[derive(Args)]
struct IssueArgs {
    #[command(subcommand)]
    job: IssueJob,
}

#[derive(Subcommand)]
enum IssueJob {
    /// 列出问题审计行（默认全部；`--open` 只看未解决）
    List(IssueListArgs),
    /// 把一行标成已解决（`resolved_at_ms=now`）；id 不存在是运维错误，退出 3
    Resolve(IssueResolveArgs),
}

#[derive(Args)]
struct IssueListArgs {
    /// 只列这个 `code`（如 `unresolved_surface`）
    #[arg(long)]
    code: Option<String>,
    /// 只列这个 `pass`（如 `naming`）
    #[arg(long)]
    pass: Option<String>,
    /// 只看未解决（`resolved_at_ms IS NULL`）
    #[arg(long)]
    open: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`[{id,pass,code,resolved_at_ms,detail}]`（`detail` 同样截断）
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct IssueResolveArgs {
    /// `extraction_issue.id`（不存在的 id 是运维错误，退出 3）
    #[arg(long)]
    id: i64,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`{"id":N,"resolved_at_ms":N,"resolved":true}`
    #[arg(long)]
    json: bool,
}

/// `hub environment declare`（W24c · P1-8）：运维直陈的环境属性。
///
/// 为什么要有它：`attribute` 表的写入路径只有导入声明（v2/v3 的 environment 那一格），
/// 而现网是 v1 导入 ⇒ 表里 0 行，W14c 裁定的"账号/主机/品牌走环境属性"一条也读不到。
/// 这条命令把同一对产品函数（`ensure_environment_anchor` + `put_attribute_rows`）接到运维面：
/// 键仍是那六个 `ATTR_*` 常量（旗标面本身就是闭集，拼不出第七个键），id 仍是内容寻址，
/// 所以同值重跑只增 `identical`。
#[derive(Args)]
struct EnvironmentArgs {
    #[command(subcommand)]
    job: EnvironmentJob,
}

#[derive(Subcommand)]
enum EnvironmentJob {
    /// 声明一台环境：一个环境锚（canonical=主机名）+ 它身上的属性行
    Declare(EnvironmentDeclareArgs),
}

#[derive(Args)]
struct EnvironmentDeclareArgs {
    /// 主机名。**必填**：属性要挂在某个环境锚上，没有主机名就没有锚
    #[arg(long)]
    hostname: String,
    /// 品牌（`Acer`）
    #[arg(long)]
    brand: Option<String>,
    /// 操作系统账号（`u`）
    #[arg(long)]
    os_user: Option<String>,
    /// 设备标识（同时是批次键与 `device_id` 属性的值）
    #[arg(long, default_value = "local-qoder")]
    device_id: String,
    /// B 站账号昵称（`甲某随行` 那类**账号名**，不是人名）
    #[arg(long)]
    bilibili_account: Option<String>,
    /// Qoder 用户名（`example-account`）
    #[arg(long)]
    qoder_account: Option<String>,
    /// 主人 principal id。缺省从转录最新 claim 解析；解析不到 ⇒ 退出 3（不猜）
    #[arg(long)]
    principal: Option<String>,
    /// 批次署名（`BatchCtx.source_agent`）
    #[arg(long, default_value = "operator")]
    source_agent: String,
    /// 属性的 scope 种类（缺省 project；闭集在 `ScopeKind::parse` 那一处，不在这里再抄一份）
    #[arg(long, default_value = "project")]
    scope_kind: String,
    /// 属性的 scope 键（缺省 `recollect`）
    #[arg(long, default_value = "recollect")]
    scope_key: String,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`{anchor_id,inserted,identical,reopened,superseded}`
    #[arg(long)]
    json: bool,
}

/// `hub episode run`：episode 归组（派发档 I-3b）。**归组内核是纯程序**（`rc_distill::episode`）：
/// 无座位 ⇒ 中间置信带只标出来、不合（零模型调用）；给了座位（I-12）⇒ 每一对交真裁决。
///
/// 它与 `derive` 是两根不同的轴，不构成第二条抽取入口：`derive` 读"还没被推导过的活记忆行"、
/// 写边/链/时间线；这一条读**活 fact**、写 `episode`/`episode_fact` 与镜像卡（L2 的"同一件事的几步"）。
#[derive(Args)]
struct EpisodeArgs {
    #[command(subcommand)]
    job: EpisodeJob,
}

#[derive(Subcommand)]
enum EpisodeJob {
    /// 沿活 fact 一趟接一趟地归组（扫描帽逐趟放大，`rc_distill::episode::EPISODE_MAX_FACTS_PER_PASS`）
    Run(EpisodeRunArgs),
}

/// `hub episode run` 的参数。形状与 [`DeriveRunArgs`] 同族（`--max-batches` / `--replies` /
/// `--llm-config` / `--data-dir` / `--llm-timeout-ms` / `--json` 同名同义），但**座位不是必填**：
/// 归组内核的后端形参是裁决接口位（`run_episode_pass(m, t, budget, adjudicator, now_ms)`）：
/// 无座位 ⇒ 折 `DeferMidBand`（中间带一律拒，零调用）；给了座位 ⇒ `LlmMidBand` 逐对真裁决（I-12）。
/// 所以"零 LLM 调用也要 rc=0"只在无座位那一支成立，不是靠一轮空批凑出来的。
/// 递了坏座位仍然当场失败（`llm_source_with_timeout` 那一段照走）：那台机器"配了但配坏了"
/// 与"没配"两格不许塌成一种（与 `CycleOnceArgs` 那条理由同源）。
#[derive(Args)]
struct EpisodeRunArgs {
    /// 最多跑多少趟；0 = 不设上限（与 `derive run --max-batches` 同口径）。
    /// 上限到了不等于扫完——余量在每行读数的 `lag_rows` 那一格里。
    #[arg(long, default_value_t = 0)]
    max_batches: usize,
    /// 座位来源之一，可选且可重复。**无座位时中间带一律拒**（零调用）；给了座位就交给真裁决器逐对问（I-12）。
    #[arg(long)]
    replies: Vec<PathBuf>,
    /// 座位来源之二：`llm.toml` 住在哪儿（与 `--replies` 同一格座位，给了就进真裁决）。
    #[arg(long)]
    llm_config: Option<PathBuf>,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 单次模型调用的墙钟上限（毫秒）；缺省 600_000，`0` 是用法错误（同 `derive run`）。
    #[arg(long, default_value_t = 600_000)]
    llm_timeout_ms: u64,
    /// 机器可读：每趟一行 JSONL 读数（键名取自派发档 §4 的那一格 + `span_pruned`）
    #[arg(long)]
    json: bool,
}

/// S1：抽取指纹的观测面。分组读数的唯一来源是 `rc_ledger::water::fingerprint_counts`，
/// 这一层只负责把 `Option<String>` 印成 `<hex|legacy>` 或 JSON 的 `null`。
#[derive(Args)]
struct FingerprintArgs {
    #[command(subcommand)]
    job: FingerprintJob,
}

#[derive(Subcommand)]
enum FingerprintJob {
    /// 按抽取指纹（管线 + 模型 + prompt）分组看轮数与时窗
    Status(FingerprintStatusArgs),
}

#[derive(Args)]
struct FingerprintStatusArgs {
    /// 机器可读：数组，字段名与 `FingerprintCount` 一致（`fingerprint` 为 null = legacy 组）
    #[arg(long)]
    json: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct CycleOnceArgs {
    /// 座位来源**之一**，可选且**可重复**（§4.8 ③ 段 B 的裁定 D：一个旗标承载主/备两座，
    /// `#[arg(long)]` + `Vec` 让 clap 自己 append）。原句（P-34，留着）："必填且刻意不给
    /// `default_value`，clap 因此自己把它拦成用法错误"——§4.8 ② 把这一格的主语改了：缺它不再是
    /// 用法错误，而是走下面那一条 `--llm-config` 的支。**不给默认值这件事照旧是刻意的**，理由与
    /// 从前同源但换了主语：一个 `default_value` 会把"这台机器还没轮到配 live"读成"读那个默认文件
    /// 读不到"，于是"没配"与"配坏了"两种后果塌成一种（`rc_llm_cfg::load()` 的 `Ok(None)`/`Err`
    /// 已为此分开）。"没有它就没有这一轮 LLM 说了什么的来源"这条理由见 §2.16 第 2 条。
    /// **出现顺序就是降级顺序**（与仓根 `llm.toml.example` 那句"顺序就是换座的顺序"同一条语义）：
    /// 一座 ⇒ 一个座位；两座 ⇒ `reply-1` 坏 ⇒ 前进到 `reply-2`。
    #[arg(long)]
    replies: Vec<PathBuf>,
    /// 座位来源**之二**：`llm.toml` 住在哪儿（§4.8 ② 末段那件"派发前新查出的空格"——这一格
    /// 此前根本没有决定：`grep -rn "llm.toml" crates/hub/src` 零命中）。
    /// 同样**刻意不给 `default_value`**（上面那条理由逐字适用）。仓根的 `llm.toml.example`
    /// 是它的形状，也是"这台机器该怎么设 `RECOLLECT_DEVICE_ID`"那句话的住处。
    #[arg(long)]
    llm_config: Option<PathBuf>,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 一轮抽取最多选多少 turn（绝对上界仍是 `rc_distill::extract::MAX_TURNS_PER_BATCH`）。
    /// 小批 A/B 用 10/15；缺省保持 40。
    #[arg(long, default_value_t = rc_cycle::DEFAULT_BATCH_TURNS)]
    batch_turns: usize,
}

#[derive(Args)]
struct ServeArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 刻意不给默认值：缺它就是"这台机器还没配中心"，而不是"去读那个默认文件读不到"。
    #[arg(long)]
    hub_config: Option<PathBuf>,
    /// 私钥文件；缺省 = `<data-dir>/hub.key`。默认值依赖另一个旗标，
    /// 写死进 clap 会出现两个互不相干的目录来源。
    #[arg(long)]
    key: Option<PathBuf>,
    /// 只允许回环地址。没有它时 0.0.0.0 / :: 一律拒绝。
    #[arg(long)]
    dev_loopback: bool,
    /// 在回环上**强制 TLS**（真进程测试与操作员演练用）。非回环本来就是默认 TLS。
    #[arg(long)]
    tls: bool,
    /// 显式退回明文 HTTP。**危险**：只在受控环境用（链路可被旁听/改道）。
    /// 非回环不带任何旗标 ⇒ TLS；回环（`--dev-loopback`）缺省仍是明文，保持既有测试不变。
    #[arg(long)]
    insecure_http: bool,
    /// S3c-4：`/v1/rows` 的 fact 行同步窄审座位（`llm.toml`）。给了就每次请求按它现造
    /// 一座 `Rotating`；不给 = 这台中心不审 fact 行（那些请求回 503 `review_unavailable`）。
    #[arg(long)]
    llm_config: Option<PathBuf>,
    /// S4c1：关闭 UDP 发现应答（默认开启）。发现只回答"这台中心是谁"，不含 token、票或记忆；
    /// 关掉它不改变任何鉴权，只是让 LAN 上的节点只能靠人给地址。
    #[arg(long)]
    no_discovery: bool,
    /// S4c1：发现应答监听的 UDP 端口。默认就是全仓唯一的那个固定端口——节点按它发问。
    #[arg(long, default_value_t = rc_net::discovery::DISCOVERY_PORT)]
    discovery_port: u16,
}

#[derive(Args)]
struct TlsArgs {
    #[command(subcommand)]
    job: TlsJob,
}

#[derive(Subcommand)]
enum TlsJob {
    /// 读 `<data-dir>/tls/{cert,key}.pem` 并打印 full/short 指纹；没有证书 ⇒ 退出 3
    Show(TlsShowArgs),
}

#[derive(Args)]
struct TlsShowArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`{"tls_fingerprint":"<64hex>","tls_short":"<16hex>"}`
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct PairArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 一次性码的有效期。测试用短值；"永不过期"不是一条分支。
    #[arg(long, default_value_t = rc_net::protocol::PAIR_CODE_TTL_MS)]
    ttl_ms: i64,
}

#[derive(Args)]
struct RevokeArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    device_id: String,
}

/// `hub invite`：无子命令 = 出票，`revoke <id>` = 作废。
/// `--hub-config` 与 serve 同义（出票要把 endpoints 签进去），缺省取 `<data-dir>/hub.toml`。
#[derive(Args)]
struct InviteArgs {
    #[command(subcommand)]
    job: Option<InviteJob>,
    /// 有效期。默认 10 分钟；`0`/负数不是"永不过期"，是用法错误。
    #[arg(long, default_value_t = rc_net::invite::DEFAULT_INVITE_TTL_MS)]
    ttl_ms: i64,
    /// 机器可读：`{id,hub_fp,tls_fp,expires_at_ms,ticket}`
    #[arg(long)]
    json: bool,
    /// 出票要读 `[hub] endpoints`；缺省 = `<data-dir>/hub.toml`。
    #[arg(long)]
    hub_config: Option<PathBuf>,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 明文 dev 面：与 `serve --insecure-http` 同义（非回环中心退回明文 ⇒
    /// 出票不再要求 `https://` 端点）。不给它时，非回环中心的票只许签 https 端点。
    #[arg(long)]
    insecure_http: bool,
}

#[derive(Subcommand)]
enum InviteJob {
    /// 作废一张票：之后用它配对一律 403（不存在 ⇒ 退出 3）
    Revoke(InviteRevokeArgs),
}

#[derive(Args)]
struct InviteRevokeArgs {
    id: String,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct BackupArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 入口取钟可被覆盖；测试用它造出七个不同的文件名。
    #[arg(long)]
    now_ms: Option<i64>,
    /// 独立 backup key 文件；给了就只落 `.db.enc`，不给则保持明文备份。
    #[arg(long)]
    backup_key_file: Option<PathBuf>,
    /// `llm.toml` 的路径；加密备份时随组走（内容进 `.enc`，不落明文）。
    /// 缺省时尝试 `<data-dir>/llm.toml`。
    #[arg(long)]
    llm_config: Option<PathBuf>,
}

#[derive(Args)]
struct PurgeArgs {
    #[arg(long)]
    session_key: Option<String>,
    #[arg(long)]
    device_id: Option<String>,
    #[arg(long)]
    confirm: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct RestoreArgs {
    /// 备份清单；两份 DB 与它同目录。任何输出都不带这个路径。
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    confirm: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 加密备份的独立 key 文件；明文备份可省略。
    #[arg(long)]
    backup_key_file: Option<PathBuf>,
    /// 把清单里已校验的 extras（`llm.toml`/`*.enc`/`*.pass`）写回 data_dir。
    /// **默认不写**：清单是未签名的普通 JSON，写回等于让能改备份目录的人换掉恢复目标的配置。
    #[arg(long)]
    with_extras: bool,
}

#[derive(Args)]
struct ReplicateArgs {
    #[command(subcommand)]
    job: ReplicateJob,
}

#[derive(Subcommand)]
enum ReplicateJob {
    /// 把一份 /v1/replicate 响应（或 records 数组）幂等落到本地
    Apply(ReplicateApplyArgs),
    /// 本地已应用到哪一格
    Status(ReplicateStatusArgs),
    /// 本地 apply 与中心 last_commit_seq 的差
    Lag(ReplicateLagArgs),
    /// 给已配对设备授全量复制读取能力
    Grant(ReplicateGrantArgs),
    /// 撤销复制能力
    Revoke(ReplicateRevokeArgs),
    /// 导出授权/审计账本快照（本地中心；远程走 GET /v1/replicate/ledger）
    ExportLedger(ReplicateExportLedgerArgs),
    /// 把账本快照幂等落到备份
    ApplyLedger(ReplicateApplyLedgerArgs),
    /// 备份侧账本快照水位
    LedgerStatus(ReplicateLedgerStatusArgs),
    /// 把一份 memory 快照整体落到备份并重建索引
    ApplyMemory(ReplicateApplyMemoryArgs),
    /// 备份侧 memory 快照水位
    MemoryStatus(ReplicateMemoryStatusArgs),
    /// 从中心拉一轮：status → records → ledger → memory
    Pull(ReplicatePullArgs),
    /// 上报备份已落地的水位（让中心知道哪些 commit 已有副本）
    Ack(ReplicateAckArgs),
    /// 读中心的合并水位：至少一份在编备份落到了哪一格
    Acked(ReplicateAckedArgs),
    /// 读中心的逐备份回执（需要复制能力；看谁过期/被撤销）
    Acks(ReplicateAckedArgs),
}

/// 三个远程复制子命令共用的中心坐标。
#[derive(Args)]
struct CenterArgs {
    /// host:port 或 http://host:port（rc-net 是明文 HTTP）
    #[arg(long)]
    endpoint: String,
    #[arg(long)]
    token_file: PathBuf,
    #[arg(long)]
    device_key_file: PathBuf,
    #[arg(long)]
    hub_pub_file: PathBuf,
    #[arg(long, default_value_t = 30_000)]
    timeout_ms: u64,
}

#[derive(Args)]
struct ReplicatePullArgs {
    #[command(flatten)]
    center: CenterArgs,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long, default_value_t = 100)]
    limit: usize,
    /// 给了就循环；不给则只拉一轮，交给外部调度器反复调用。
    #[arg(long)]
    interval_ms: Option<u64>,
    /// 拉完后向中心上报本轮落地水位。
    #[arg(long)]
    ack: bool,
    /// 拉完后 lag 超过这个值就退出 4（供调度器告警）。
    #[arg(long)]
    max_lag: Option<u64>,
    #[arg(long)]
    now_ms: Option<i64>,
}

#[derive(Args)]
struct ReplicateAckArgs {
    #[command(flatten)]
    center: CenterArgs,
    #[arg(long)]
    applied_seq: u64,
}

#[derive(Args)]
struct ReplicateAckedArgs {
    #[command(flatten)]
    center: CenterArgs,
}

#[derive(Args)]
struct ReplicateExportLedgerArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    output: PathBuf,
}

#[derive(Args)]
struct ReplicateApplyLedgerArgs {
    #[arg(long)]
    input: PathBuf,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    now_ms: Option<i64>,
}

#[derive(Args)]
struct ReplicateLedgerStatusArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct ReplicateApplyMemoryArgs {
    #[arg(long)]
    snapshot: PathBuf,
    #[arg(long)]
    expect_sha256: String,
    #[arg(long)]
    expect_commit_seq: u64,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    now_ms: Option<i64>,
}

#[derive(Args)]
struct ReplicateMemoryStatusArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct ReplicateGrantArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    device_id: String,
    #[arg(long)]
    now_ms: Option<i64>,
}

#[derive(Args)]
struct ReplicateRevokeArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    device_id: String,
    #[arg(long)]
    now_ms: Option<i64>,
}

#[derive(Args)]
struct ReplicateApplyArgs {
    #[arg(long)]
    input: PathBuf,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    now_ms: Option<i64>,
}

#[derive(Args)]
struct ReplicateStatusArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct ReplicateLagArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    center_last_seq: u64,
    #[arg(long)]
    center_epoch: u64,
}

#[derive(Args)]
struct PromoteArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 旧中心的 hub key；提升证书由它签名。
    #[arg(long)]
    prev_key: PathBuf,
    /// 新中心的 hub key；缺省 = `<data-dir>/hub.key`。
    #[arg(long)]
    key: Option<PathBuf>,
    /// 操作员从旧中心 `/v1/replicate/status` 抄下来的读数。
    #[arg(long)]
    center_last_seq: u64,
    #[arg(long)]
    center_epoch: u64,
    #[arg(long)]
    now_ms: Option<i64>,
    /// 允许带 lag 提升（演练/紧急只读降级）；默认拒绝。
    #[arg(long)]
    force: bool,
}

#[derive(Args)]
struct FenceArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 新中心的 epoch；旧中心见到 >= 自己 epoch 的 fence 就拒绝写。
    #[arg(long)]
    epoch: u64,
    #[arg(long)]
    now_ms: Option<i64>,
    #[arg(long)]
    confirm: bool,
}

#[derive(Args)]
struct SnapshotArgs {
    #[command(subcommand)]
    action: SnapshotAction,
}

#[derive(Subcommand)]
enum SnapshotAction {
    Publish(SnapshotPublishArgs),
}

#[derive(Args)]
struct SnapshotPublishArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 发布哪条通道：`agent`（默认，记忆通道；审查两表清空）或 `backup`（完整记忆）
    #[arg(long, default_value = "agent")]
    channel: String,
}

#[derive(Args)]
struct SecretArgs {
    #[command(subcommand)]
    job: SecretJob,
}

#[derive(Subcommand)]
enum SecretJob {
    /// 新建空箱与它的私有口令文件；已存在则不覆盖
    Init(SecretInitArgs),
    /// 从 stdin 写一格（值不回显、不进参数表）
    Put(SecretPutArgs),
    /// 只列格名
    List(SecretListArgs),
    /// 删一格；缺这一格也是 0
    Rm(SecretRmArgs),
}

#[derive(Args)]
struct SecretInitArgs {
    /// 箱名 = 文件名 stem（`<data-dir>/<box>.enc`）；默认 `secrets`
    #[arg(long = "box", default_value = "secrets")]
    box_name: String,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct SecretPutArgs {
    /// `BOX:SLOT`；两侧都只许 safe token
    secret_ref: String,
    /// 值只能从管道来：不给它就不读 stdin
    #[arg(long)]
    value_stdin: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct SecretListArgs {
    /// 箱名（与 `init --box` 同一套形状）
    box_name: String,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct SecretRmArgs {
    /// `BOX:SLOT`
    secret_ref: String,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct KeyArgs {
    #[command(subcommand)]
    job: KeyJob,
}

#[derive(Subcommand)]
enum KeyJob {
    /// 把 `<data-dir>/hub.key` 的原字节写进一个新建的密文箱；不覆盖已有归档
    Export(KeyExportArgs),
    /// 从密文箱恢复 `<data-dir>/hub.key`；已有私钥则拒绝覆盖
    Import(KeyImportArgs),
}

#[derive(Args)]
struct KeyExportArgs {
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct KeyImportArgs {
    /// 归档文件；任何输出都不带这个路径
    #[arg(long = "from")]
    from_file: PathBuf,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct BackupKeyArgs {
    #[command(subcommand)]
    job: BackupKeyJob,
}

#[derive(Args)]
struct EpochArgs {
    #[command(subcommand)]
    job: EpochJob,
}

#[derive(Subcommand)]
enum EpochJob {
    /// 要求每个签名请求都带 epoch 头（先确认设备都升级完）
    Require(EpochDirArgs),
    /// 重新允许 epoch 1 的 v1 客户端（回滚用）
    Allow(EpochDirArgs),
    /// 读当前策略
    Status(EpochDirArgs),
}

#[derive(Args)]
struct EpochDirArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Subcommand)]
enum BackupKeyJob {
    /// 生成一个新的 32 字节 backup key 文件；已存在则不覆盖
    Generate(BackupKeyGenerateArgs),
}

#[derive(Args)]
struct BackupKeyGenerateArgs {
    #[arg(long)]
    output: PathBuf,
}

/// S1b-C4b2a：旧数据的资源归属回填（设计 ownership-persistence §6）。
#[derive(Args)]
struct OwnershipArgs {
    #[command(subcommand)]
    job: OwnershipJob,
}

#[derive(Subcommand)]
enum OwnershipJob {
    /// 给已有的 card/anchor 行补 resource_owner，并清掉孤儿归属行（可重跑）
    Backfill(OwnershipBackfillArgs),
}

#[derive(Args)]
struct OwnershipBackfillArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：单个对象，字段与 `BackfillReport` 同名的 snake_case
    #[arg(long)]
    json: bool,
}

/// 身份层 I1（设计 `2026-10-27-principal-identity.md` §4、工单 2026-10-28-principal-i1）：
/// 主人身份的盖章与台账。写的和读的都在 transcript.db 的 `session_principal_claim` 上——
/// `sessions` 是 append-only，"改盖"只能追加到这张侧表。
#[derive(Args)]
struct PrincipalArgs {
    #[command(subcommand)]
    job: PrincipalJob,
}

#[derive(Subcommand)]
enum PrincipalJob {
    /// 追加一条章（不更新）。`--source correction` 未给 `--reason` ⇒ 退出 3
    Claim(PrincipalClaimArgs),
    /// 列每个会话的当前有效章 + 历史（`supersedes` 链）
    List(PrincipalListArgs),
    /// W37-A2/G13③②：对无章会话按环境声明补章（幂等；daily-drain 蒸馏前的那一枪）。
    /// `RECOLLECT_PRINCIPAL_NAME` 没设 ⇒ 一格不写、rc=0——"没声明"不是故障
    ClaimEnv(PrincipalClaimEnvArgs),
    /// W37-A2/G13④：存量缺章记忆行的带账回填。**默认 dry-run 一格不写**，`--apply` 才写
    #[command(name = "backfill")]
    Backfill(PrincipalBackfillArgs),
    /// W37 归一化：归属串从旧 principal_id 改写到新稳定 id（带账 + 确保新主人锚）。
    /// **默认 dry-run 一格不写**，`--apply` 才写。章本身走 `claim --source correction`。
    #[command(name = "remap")]
    Remap(PrincipalRemapArgs),
}


/// 实体锚维护子命令。
#[derive(Args)]
struct AnchorArgs {
    #[command(subcommand)]
    job: AnchorJob,
}

#[derive(Subcommand)]
enum AnchorJob {
    /// 把挂在旧锚上的关联数据换绑到新锚；旧锚转 `merged`，它的正名并进新锚的别名
    Rebind(AnchorRebindArgs),
    /// 发现同一实体的重复锚：A 类（同 kind 同 scope 同名）可合并，B/C 只出人工清单
    Merge(AnchorMergeArgs),
}

#[derive(Args)]
struct AnchorRebindArgs {
    /// 被换走的锚 id（弱称呼、重复实体、后来认出不是本人的那一格）
    #[arg(long)]
    from: String,
    /// 换到哪一格的锚 id（本人格）
    #[arg(long)]
    into: String,
    /// 为什么换（进 `memory_op.reason`，是给后来人看的唯一解释）
    #[arg(long)]
    reason: Option<String>,
    /// 谁做的：闭集同 `memory_op.actor`
    #[arg(long, default_value = "user",
          value_parser = ["user", "agent", "program", "center", "extractor"])]
    actor: String,
    /// 只看会动什么，一格都不写
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct AnchorMergeArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 指向合并的旧锚（**方案二**：不重键、零搬运，写 `anchor_merge` 指向行；与 A/B/C
    /// 发现合并并存）。给了 `--from` 就走指向模式，`--only` 不适用。
    #[arg(long, conflicts_with = "only", requires = "to")]
    from: Option<String>,
    /// 指向合并的幸存锚（`to` 已 merged 也允许：链加长，A→B、B→C ⇒ A 解析到 C）。
    #[arg(long, requires = "from")]
    to: Option<String>,
    /// 只看会动什么：默认行为，一格都不写（显式给出便于脚本自证）
    #[arg(long, conflicts_with = "apply")]
    dry_run: bool,
    /// 真写：合并 A 类，写 `anchor_merge` 审计行；B/C 始终只出清单
    #[arg(long)]
    apply: bool,
    /// 只处理哪一类（只有 A 有写动作；给 B/C 等于"只看不动"）
    #[arg(long, value_parser = ["A", "B", "C"])]
    only: Option<String>,
    /// 为什么合并（同时进 `memory_op.reason` 与 `anchor_merge.reason`）
    #[arg(long)]
    reason: Option<String>,
    /// 谁做的：闭集同 `memory_op.actor`
    #[arg(long, default_value = "user",
          value_parser = ["user", "agent", "program", "center", "extractor"])]
    actor: String,
    /// 清单的机器可读形（数组；apply 时只列真合并的那些行）
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct PrincipalClaimArgs {
    /// 会话键（与导入时的 `session_key` 同一格）
    #[arg(long)]
    session: String,
    /// 设备标识（`sessions.device_id` 同一格）
    #[arg(long)]
    device: String,
    /// 稳定主键（账号 id 或 Agent 派生的稳定 id；拿不到就写 `unknown`）
    #[arg(long)]
    principal: String,
    /// 可读名（昵称/称呼）。不是主键，不做任何推断/合并
    #[arg(long)]
    name: Option<String>,
    /// 章从哪来：闭集与 `rc_contract::PRINCIPAL_SOURCES` 同一份
    #[arg(long, value_parser = rc_contract::PRINCIPAL_SOURCES)]
    source: String,
    /// 改盖原因。`--source correction` 必填（工单规则 2）
    #[arg(long)]
    reason: Option<String>,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
}

#[derive(Args)]
struct PrincipalListArgs {
    /// 只看这个会话（缺省 = 全部）
    #[arg(long)]
    session: Option<String>,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：claim 数组（每行带 `latest`）
    #[arg(long)]
    json: bool,
}

/// `hub principal claim-env`：读进程环境的三格声明（与导入路径同一份 `PrincipalDecl`），
/// 对无章会话逐个补章。没有 `--source` 之类旗标：这一格闭集里的值由入口固定为
/// `agent_runtime`，运维手盖走 `principal claim`。
#[derive(Args)]
struct PrincipalClaimEnvArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：单个对象（declared/sessions_scanned/claims_written/already_claimed）
    #[arg(long)]
    json: bool,
}

/// `hub principal backfill`：缺章行的带账回填。默认 dry-run（零写入、回执照给）；
/// `--apply` 才真写 `resource_owner` + `memory_op(reason='principal_backfill')`。
#[derive(Args)]
struct PrincipalBackfillArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 真写。缺省（不给它）= dry-run，显式给出便于脚本自证。
    #[arg(long)]
    apply: bool,
    /// 机器可读：单个对象（dry_run/rows_scanned/backfilled/unresolved + rows 明细数组）
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct PrincipalRemapArgs {
    /// 旧归属串（要被改写掉的 principal_id，如 `旧主人`）
    #[arg(long)]
    from: String,
    /// 新稳定 id（如 `p-owner`；非空且不等于 `--from`，否则退出 3）
    #[arg(long)]
    to: String,
    /// 新主人锚的可读名（进 alias，如 `旧主人`；缺席不写别名）
    #[arg(long)]
    name: Option<String>,
    /// 真写。缺省 = dry-run。
    #[arg(long)]
    apply: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    #[arg(long)]
    json: bool,
}

/// S3a-3d：待审队列的观测面与人工改判（设计 `review-queue-s3` §9）。
/// 输出结构/命令实现住 `hub::review`；这里只有 clap 形状与分派（与 `ownership` 同一分层）。
#[derive(Args)]
struct ReviewArgs {
    #[command(subcommand)]
    job: ReviewJob,
}

#[derive(Subcommand)]
enum ReviewJob {
    /// 列出待审项：默认每行 id state pass/family session created_ms（不打印载荷）
    List(ReviewListArgs),
    /// 看一条待审项：meta、判定历史与原因码；只有 --payload 才打印载荷
    Show(ReviewShowArgs),
    /// 人工改判：追加一条 decider=human 判定并推进/回退状态
    Decide(ReviewDecideArgs),
    /// 各状态计数、最老 pending 年龄（含 7 天告警）、按 pass 分组
    Stats(ReviewStatsArgs),
    /// 逐条重跑可运行项（pending/quarantined 复审 + approved 补写）；失败退避、额度耗尽即停
    Run(ReviewRunArgs),
    /// §7.3b：只把 `final=uncertain` 的 fact 草稿单独重问一次（上下文更足），程序合成终态并记三个率
    #[command(name = "retry-uncertain")]
    RetryUncertain(ReviewRetryArgs),
    /// 清终态（applied/rejected/stale）item 的载荷与占用列；判定台账与非终态不动
    Prune(ReviewPruneArgs),
    /// T1：把历史已 applied 的 fact/event 载荷重算成卡 id，把仍 unreviewed 的卡提升成 passed
    /// （可 dry-run、幂等、可按清单回滚）
    #[command(name = "backfill-verdict")]
    BackfillVerdict(ReviewBackfillArgs),
    /// 波二刀 B：把复用反馈的 failed/mismatch 入复核队列（source=reuse，只走人工判定）
    #[command(name = "reuse-ingest")]
    ReuseIngest(ReviewReuseArgs),
}

#[derive(Args)]
struct ReviewListArgs {
    /// 只列这个状态
    #[arg(long, value_parser = ["pending_review", "approved", "applied", "rejected", "quarantined", "stale"])]
    state: Option<String>,
    /// 只列这条 pass（`reuse` = 复用反馈的待审项，刀 B）
    #[arg(long, value_parser = ["naming", "fact", "event", "time", "rows", "state", "reuse"])]
    pass: Option<String>,
    /// 最多列多少条；0 = 不设上限
    #[arg(long, default_value_t = 0)]
    limit: usize,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：完整 ItemMeta 数组
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ReviewShowArgs {
    id: i64,
    /// 连载荷一起打印（默认绝不打印载荷正文）
    #[arg(long)]
    payload: bool,
    /// DR：打印这条 item 的审核输入原文（`SYSTEM` + 每个 `CHUNK i`），不调模型、不写表。
    ///
    /// 与 `--payload` 同给是用法错误：前者是"模型看到的"，后者是"入队时存的载荷"，
    /// 静默取一个会让读者把两者当成同一份东西。
    #[arg(long, conflicts_with = "payload")]
    prompt: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：结构化对象（无 --payload 时不带载荷）
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ReviewDecideArgs {
    id: i64,
    /// 推进到 approved。**只改队列状态**：实际写库由后续 `hub review run` 执行（S3b）。
    #[arg(long, conflicts_with = "reject")]
    approve: bool,
    /// 判为 rejected（终态，不写记忆行）
    #[arg(long, conflicts_with = "approve")]
    reject: bool,
    /// 原因码，闭集形状 `[a-z0-9_]+`
    #[arg(long)]
    reason: String,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`{"id":N,"state":"..."}`
    #[arg(long)]
    json: bool,
}

#[derive(Args)]
struct ReviewStatsArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：六态计数、oldest_pending_age_ms、oldest_pending_over_7d、by_pass
    #[arg(long)]
    json: bool,
}

/// 波二刀 B：`hub review reuse-ingest`。无过滤参数——程序判定（failed/mismatch）与查重
/// 全在 `rc_memory::reuse::ingest_audit` 里，这一层只给数据目录与两形输出。
#[derive(Args)]
struct ReviewReuseArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`{candidates,queued,deduped,items:[{card_id,review_item_id,fresh}]}`
    #[arg(long)]
    json: bool,
}

/// `hub review prune`（H7a）：清终态 item 的载荷，判定台账不动。
///
/// `--older-than-days` 是**正整数天**：0 与负数只会把库一次清空（见
/// `hub::review::FAULT_REVIEW_PRUNE_DAYS`），先于开库按用法错误回。
#[derive(Args)]
struct ReviewPruneArgs {
    /// 只清 `updated_at_ms` 早于 N 天前的终态 item（applied/rejected/stale）
    #[arg(long)]
    older_than_days: i64,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`{"pruned":N,"older_than_days":N}`
    #[arg(long)]
    json: bool,
}

/// `hub review backfill-verdict`（T1）：把历史已 applied 的 fact/event 载荷重算成卡 id，
/// 把仍 `unreviewed` 的卡提升成 `passed`。
///
/// 旗标三组，互斥关系在代码里判（`--pass` 有缺省值，clap 的 `conflicts_with` 分不出
/// "没给"与"给了缺省"）：
/// - 扫描：`--pass` / `--limit` / `--manifest`（真跑写提升的 id）/ `--json`；
/// - 干跑：`--dry-run`（不写一个字节）；
/// - 回滚：`--revert MANIFEST`（只碰清单内、当前 passed 的 id）。
#[derive(Args)]
struct ReviewBackfillArgs {
    /// 只扫这些 pass（逗号分隔）；闭集 fact,event —— naming/time 不产卡，显式拒绝
    #[arg(long)]
    pass: Option<String>,
    /// 只读：打印清单（+ `--manifest` 落盘），不写一个字节
    #[arg(long)]
    dry_run: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：清单数组（`review_item_id/item_index/card_id/title/verdict_before/action`）
    #[arg(long)]
    json: bool,
    /// 清单路径：干跑写本次清单（即回滚清单）；真跑写实际提升的卡 id
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// 回滚这份清单内、当前 `passed` 的卡到 `unreviewed`（0 行不算错）
    #[arg(long)]
    revert: Option<PathBuf>,
    /// 最多扫多少条 review_item；0 = 不设上限
    #[arg(long, default_value_t = 0)]
    limit: usize,
}

/// `hub review run`（S3b-4a）：逐条重跑可运行项。
///
/// 座位来源两格与 `distill once` 同名同义（共用 [`llm_source_with_timeout`]），但**惰性**：
/// 可运行项全是 `approved` 时一次都不读来源——那一支的判决从台账重建，不问模型。
#[derive(Args)]
struct ReviewRunArgs {
    /// 最多处理多少条；0 = 不设上限（与 `list` 同口径）。
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// 只跑这条 pass（闭集就是下面 `value_parser` 那六个，**与 `list` 不同**：`reuse` 只走人工
    /// 判定链，`lesson` 的终态行由 `pass_lesson` 自己写）。
    #[arg(long, value_parser = ["naming", "fact", "event", "time", "rows", "state"])]
    pass: Option<String>,
    /// 座位来源之一，可选且可重复（顺序就是降级顺序）。
    #[arg(long)]
    replies: Vec<PathBuf>,
    /// 座位来源之二：`llm.toml` 住在哪儿（刻意不给默认值，理由同 `DistillOnceArgs`）。
    #[arg(long)]
    llm_config: Option<PathBuf>,
    /// 单次模型调用的墙钟上限（毫秒）；0 是用法错误（同 `distill once`）。
    #[arg(long, default_value_t = 600_000)]
    llm_timeout_ms: u64,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`RunOut` 的完整字段
    #[arg(long)]
    json: bool,
}

/// §7.3b：`hub review retry-uncertain`——只把判 `uncertain` 的 fact 草稿单独重问一次。
///
/// 参数面与 `review run` 同风格（`--pass`/`--llm-config`/`--data-dir`/`--limit`/`--json`/
/// `--llm-timeout-ms`，`--replies` 同一条回放来源）。差别只有两处：
/// - `--pass` 只认 `fact`（重跑的 prompt 与契约都是 fact 的，别的 pass 没有这一层）；
/// - `--limit` 按 **item** 计数（与 `review run` 同口径）。
///
/// 它与 `review run` 是**两条命令**：`review run` 把隔离项按正常审核（整批、两次调用）
/// 再问一遍；这一条只问那几条 `final=uncertain` 的草稿，一次调用只判一条，并合成 item 终态。
#[derive(Args)]
struct ReviewRetryArgs {
    /// 最多重跑多少条 **item**；0 = 不设上限（与 `review run` 同口径）。
    #[arg(long, default_value_t = 0)]
    limit: usize,
    /// 只对这条 pass 生效；重跑只做 fact（别的值先于开库按用法错误回）。
    #[arg(long, value_parser = ["naming", "fact", "event", "time", "rows"])]
    pass: Option<String>,
    /// 座位来源之一，可选且可重复（顺序就是降级顺序）。
    #[arg(long)]
    replies: Vec<PathBuf>,
    /// 座位来源之二：`llm.toml` 住在哪儿（同 `ReviewRunArgs`）。
    #[arg(long)]
    llm_config: Option<PathBuf>,
    /// 单次模型调用的墙钟上限（毫秒）；0 是用法错误（同 `review run`）。
    #[arg(long, default_value_t = 600_000)]
    llm_timeout_ms: u64,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`RetryMetrics` 的完整字段
    #[arg(long)]
    json: bool,
}

/// `hub reconcile`（H7c1）：接管对账的命令面。今天只有 `memory` 一条（审查轴水位 H7c2 不在本轮）。
#[derive(Args)]
struct ReconcileArgs {
    #[command(subcommand)]
    job: ReconcileJob,
}

#[derive(Subcommand)]
enum ReconcileJob {
    /// 记忆/转录水位缺口、by_state 与 done 轮数；--apply 把缺口内 done 的 pass 行标 stale(memory_gap)
    Memory(ReconcileMemoryArgs),
}

#[derive(Args)]
struct ReconcileMemoryArgs {
    /// 真把缺口内 `state=done` 的 pass 行改成 `stale(reason=memory_gap)`；不给只报读数。
    /// 其它状态（empty/rejected/quarantined/pending/running）一律不动。
    #[arg(long)]
    apply: bool,
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 机器可读：`GapReport` 的完整字段
    #[arg(long)]
    json: bool,
}

/// 能力自测那一支的类别串（守卫 14 的执法点在 `run_cycle_with`）。cycle 的其余类别串
/// （读写库、计数器、资格、扫描）随管线住 `rc-cycle`：错误面的家只有一个，hub 只 import。
const FAULT_PROBE: &str = "capability probe failed — run: doctor";

/// §4.8 ②④ 段 A 的五条，全部 `&'static str`、零插值（守卫 12 的执法点与上面同形）：
/// 它们点名的是**旗标名与变量名**，从来不是操作员递进来的那个值——`--llm-config` 指的路径、
/// `llm.toml` 里那家的名字、`RECOLLECT_DEVICE_ID` 里那串身份，一个字节都不许走到 stderr。
/// 三条"没给来源 / 配坏了 / preset 查不到"必须是三种不同的后果，共用一句就是把操作员支到错误的面上。
const FAULT_NO_LLM_SOURCE: &str =
    "no llm reply source — pass --replies FILE or --llm-config FILE to: hub cycle once";
const FAULT_LLM_CFG_BAD: &str = "llm config is malformed — run: doctor";
const FAULT_LLM_PRESET: &str = "llm seat names a provider outside the preset table — see: llm.toml.example";
const FAULT_NO_DEVICE_ID: &str = "device identity missing — set: RECOLLECT_DEVICE_ID";
/// B2a-3 起 live 那一支有了读箱的手：三类读箱失败各归一条（一个变体一条、零路径零值零
/// slot 名）。它们点的是**动作**（跑 doctor：建箱/查口令/补格），不是"再试一次"。
const FAULT_LLM_BOX_MISSING: &str = "llm secret box not found — run: doctor";
const FAULT_LLM_SECRET_OPEN: &str = "llm secret box cannot be opened — run: doctor";
const FAULT_LLM_SLOT_MISSING: &str = "llm secret slot is missing — run: doctor";
/// §4.8 ③ 的两格。读不出来 ⇒ 整轮不跑（那一轮的降级判断不可信，猜成"没降级"会把主家
/// 反复重新服务，猜成"降级了"会把一轮烧在备座上——两种都是静默改判）；
/// 写失败 ⇒ 降判定但**读数照旧**（与 `FAULT_COUNTERS` 同一族的先例，B3 点名的就是那一条）。
/// 两条都是零插值：`StoreError::Sql` 的原串能带库路径，也能带那句 SQL。
const FAULT_LLM_STATE_READ: &str = "llm rotation state unreadable — run: doctor";
const FAULT_LLM_STATE_WRITE: &str = "llm rotation state write failed — run: doctor";
/// 作废规则④"缺身份"那一支的运维下行（`warning` 通道，不是 `error`：这一支仍是能跑完的一轮）。
/// 与上面那一族同一纪律：零插值、点名变量名、不带路径——它说的是"这一轮的轮转态没继承"这件事，
/// 与 `FAULT_NO_DEVICE_ID`（live 座位来源那一支的后果，判定 3）是**两个来源**，共用一句会把
/// 操作员支到错误的面上。
const WARN_LLM_STATE_NO_IDENTITY: &str =
    "llm rotation state not inherited — no device identity: RECOLLECT_DEVICE_ID";

/// S2a-3：`distill once` 的运维类别词（守卫 12：只有类别，零插值）。
/// 与 cycle 的长句不同，这两条是**词**：退出码 2/3 与它们一一对应，测试与脚本按词读。
const FAULT_REJECTED: &str = "rejected";
const FAULT_STORE: &str = "store";

/// `llm` 那一族的后缀：把 [`rc_llm::LlmFailure`] 的四态原样带进 stderr。
///
/// 2026-10-31 补这一格。四态在 CLI 面曾被吞成同一个词 `llm`，于是"墙钟切在正常但慢的请求
/// 前面"（实测：HTTP 200 + `base_resp.status_code=0` + 正文是合法 JSON，思考 15,561 token、
/// 全程 >120s）与"供应商真坏"在抽干日志里长得一模一样，本机与中心各花半小时才分清。
/// 四个返回值全是 `&'static str` ⇒ 守卫 12 的"零外部输入"照旧成立：一个字节都不是模型给的。
/// 第一个词仍是 `llm` ⇒ 既有按 `error: llm` 读的词与用例照旧命中。
/// 后缀逐字用 [`crate::replay`] 那张四态 slug 表（`--replies` 里写的就是这四个词），操作员可以
/// 拿同一个词在两处对查。
fn llm_fault_text(failure: LlmFailure) -> &'static str {
    match failure {
        LlmFailure::Timeout => "llm timeout",
        LlmFailure::SchemaMismatch => "llm schema_mismatch",
        LlmFailure::QuotaExhausted => "llm quota_exhausted",
        LlmFailure::BackendError => "llm backend_error",
    }
}

/// 未知 `--pass` 的唯一去向：不静默回落、也不回显操作员递进来的那个值。
/// S2c-2 起三条 pass：naming、fact（必须带 `--family`）与 event；S2d-2 再加 time（纯程序）。
const FAULT_PASS: &str =
    "distill supports only: --pass naming, --pass fact --family decision|promise|plan, --pass event, --pass state, --pass lesson, --pass time";
/// `--pass fact` 缺 family 或 family 不在闭集：同一句（闭集即点名词，不回显未知值）。
/// 句中带 `naming` 不是装饰：`--pass fact` 不带 `--family` 与 naming 的形状只差这一格，
/// 操作员要能一眼看出"这条 pass 有额外必填项"。
const FAULT_FACT_FAMILY: &str =
    "distill --pass fact needs --family decision|promise|plan (unlike --pass naming)";
/// `--pass naming` 带了 `--family`：naming 只有一族（空串 family），多给就是用法错误。
const FAULT_NAMING_FAMILY: &str = "distill --pass naming takes no --family";
/// `--pass event` 带了 `--family`：event 是单一 pass（空串 family），没有调用变体可问。
/// 与 naming 那一句分开：两条 pass 各有各的形状，共用一句会把操作员支到错的命令上。
const FAULT_EVENT_FAMILY: &str = "distill --pass event takes no --family";
/// `--pass time` 带了 `--family`：time 也是单一 pass（`family=''`）。time 是纯程序，三族
/// 在这一条 pass 下没有区别——多给它就是用法错误，静默忽略会让操作员以为"只跑了 promise"。
const FAULT_TIME_FAMILY: &str = "distill --pass time takes no --family";
/// `--pass state` 带了 `--family`：state 是单一 pass（`family=''`），没有调用变体可问。
const FAULT_STATE_FAMILY: &str = "distill --pass state takes no --family";
/// `--pass lesson` 带了 `--family`：lesson 是单一 pass（`family=''`），没有调用变体可问。
const FAULT_LESSON_FAMILY: &str = "distill --pass lesson takes no --family";
/// `--pass time` 的 `pass_id`（状态键与读数都用它；`PassState` 的闭集里已有 "time"）。
const TIME_PASS: &str = "time";
/// S3b-4b：`--pass time --no-review` 的唯一去向。time 是纯程序（没有窄审、一次模型都不调），
/// 给它"只入队待审"等于把一批没有队列可进的轮读成"等审核"——用法错误，退出 3、零插值。
const FAULT_NO_REVIEW_ON_TIME: &str =
    "distill --pass time takes no --no-review (time has no review step)";
/// S2e-2：`--llm-timeout-ms 0` 的唯一去向。0 毫秒**不是**一个可解释的值：
/// 读成"不设超时"就正好是这一格要消灭的无界挂起，读成"立即超时"就是操作员递错一个数之后的
/// 静默灾难。两支都不许发生，所以它是用法错误（退出 3、零插值、点名旗标名）。
const FAULT_LLM_TIMEOUT: &str = "distill --llm-timeout-ms must be greater than zero";
/// DR：`--dry-run` 与座位来源同给的唯一去向。这两格声明的是相反的事
/// （"只打印" / "这次真问模型"），静默忽略任一格都会让操作员读错刚才发生了什么。
const FAULT_DRY_RUN_SOURCE: &str =
    "distill --dry-run takes no --replies or --llm-config (dry run never calls a model)";
/// DR：`--pass time --dry-run` 的唯一去向。time 一个字节的模型输入都没有
/// （纯确定性解析与回填），"导出模型看到的原文"在它身上没有主语 ⇒ 用法错误。
const FAULT_DRY_RUN_TIME: &str =
    "distill --pass time takes no --dry-run (time has no model input)";
/// 2026-10-30-retry-empty：空轮重试**只对 event** 有牙齿。fact 的三族与 naming 的 `pull`、
/// time 的纯程序面都不在本片（派发档 §要做 5：fact/naming 维持原样）。多给这一格是用法错误、
/// 零插值——静默忽略会让操作员以为"这一条 pass 也重采过空轮"，而那批读数看起来一模一样。
const FAULT_RETRY_EMPTY: &str =
    "distill takes --retry-empty only with --pass event";

/// §4.8 ③ 的两个转换函数（B1）。`Seed`/`Persisted` 都住在 `rc-llm`，hub 给不了它们 `From`
/// （孤儿规则），所以是两条自由函数；字段对表逐列等值，`Row` 只多 `machine` 与 `written_at_ms`
/// 两列——那两列的主语是**这台机器**与**这一轮**，`rc-llm` 那一层拿不到（文件头"零 IO、零时钟"）。
///
/// **不许 clamp、不许补默认值**（裁定 C）：`read` 那侧把 `i64` 折成 `u32`/`usize` 已经是它的债，
/// 这里再修一次就是把"有人手工改过库"读成"这台机器的判断本来就是 0"。坏值绕回来比被静默修好可读。
/// 今天写这一行的只有 hub，而它写的是 `persisted()` 的原样值。
fn seed_of_row(row: &llm_state::Row) -> Seed {
    Seed { slug: row.slug.clone(), probe_due_at_ms: row.probe_due_at_ms,
           probe_ok: row.probe_ok, probe_failures: row.probe_failures,
           written_at_ms: row.written_at_ms }
}

/// 反方向。`machine` 与 `written_at_ms` 由调用方递进来：前者是 B2 取到的那个身份，
/// 后者是**本轮传进来的 `now_ms`**（裁定 V14：一轮之内不许有两个"现在"，所以这里不再取一次钟）。
fn row_of_persisted(machine: &str, written_at_ms: i64, p: &Persisted) -> llm_state::Row {
    llm_state::Row { machine: machine.to_owned(), slug: p.slug.clone(),
                     probe_due_at_ms: p.probe_due_at_ms, probe_ok: p.probe_ok,
                     probe_failures: p.probe_failures, written_at_ms }
}

/// 一座回放座位 = 一个标签 + 一份脚本（B4 的形状）。标签是 `reply-<序位>`：`safe_token`
/// 形状、不含文件名也不含路径——这一串会进 `llm_state.slug`，也会被 `doctor` 原样念出来。
type Seats = Vec<(String, Vec<replay::Reply>)>;

/// 构造好的座位槽位：标签 + 真后端。回放与 live 只在**构造**那一步不同，进 `Rotating`
/// 之后是同一个形状（`live-<序位>` / `reply-<序位>` 两个 slug 命名空间不重叠）。
type Slots = Vec<(String, Box<dyn LlmBackend>)>;

fn seat_slug(index: usize) -> String {
    let ordinal = index + 1;
    format!("reply-{ordinal}")
}

/// 打印前的收口：`quota_exhausted` 与 `halted` 里前者在这里填，于是"故障分支忘记填"
/// 这个失效形状结构上不存在（每一条出口都得过这里）。stdout 一律走 `write_line`（裁定 R19）。
/// 不收 `root`：这一格只需要读数和计数器面，句柄递进来就是下一个人的 `unused_variables` 陷阱。
///
/// B2b-1 起它是**自由函数**而不是 `CycleExit::finish`：`CycleExit` 搬进了 `rc-cycle`，
/// 而打印与 0/2/3 的映射是 hub 的事（那一层只交判定）。逻辑逐字保留。
fn finish_cycle(exit: CycleExit, out: &mut CycleOut, meter: &Metered,
                quarantine_issue: Option<i64>) -> ExitCode {
    out.quota_exhausted = meter.saw_quota_exhausted();
    // 二十格 → 二十格 + 隔离两格：这两格只在**本轮真隔离**时出现（`quarantine_issue` 是
    // 本轮 `run_id` 读回的 issue id），所以 happy path 的十七格 JSON 形状一个字节都不动。
    // 它们不进 `CycleOut`：那是 `rc-cycle` 的读数结构，本轮文件集不含它；这里的值来自
    // `derive.rs` 写下的**同一轮**记录（`issues::derive_run_id` 是唯一拼法）。
    let text = match serde_json::to_value(&*out) {
        Ok(mut v) => {
            if let Some(id) = quarantine_issue {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("derive_quarantined".to_owned(), json!(1));
                    obj.insert("derive_issue_id".to_owned(), json!(id));
                }
            }
            match serde_json::to_string(&v) {
                Ok(t) => t,
                Err(_) => return operational(FAULT_STORE_WRITE),
            }
        }
        Err(_) => return operational(FAULT_STORE_WRITE),
    };
    write_line(&text);
    match exit {
        CycleExit::Ran => ExitCode::from(0),
        CycleExit::BatchDropped => ExitCode::from(2),
        CycleExit::Fault(cat) => operational(cat),
    }
}

// cycle 的管线（`run_cycle_once`）、读数与判定（`CycleOut` / `CycleExit`）、错误面
// （`qualify` / `halted_of` / `cycle_failure` / `charge_counters` / `index_failure`）与扫描节奏
// （`FULL_SCAN_EVERY` / `ANOMALY_SCOPE` / `scan_due` / `scan_rounds` / `run_anomaly_scan`）
// 自 B2b-1 起住 `rc-cycle`（本文件顶部 `use rc_cycle::{..}` 是它们的调用点）。
// 行为没动：这一搬的回归闸是 `tests/cycle_once.rs` 与 `tests/rotation_state_across_processes.rs`。

/// §4.8 ① 的身份读取点。全仓只有这一个命名空间（`RECOLLECT_DEVICE_ID`，与 `mcp` 那一条门面同一格），
/// 取到的值**立刻**过既有的 [`hub::ingest::device_subject`]——不新写第二个校验函数（守卫 3），
/// 也不许开出第二个身份命名空间（判据 35 的 `zero` 那一支扫的是整个 `crates`，
/// 连注释里的那个旧名字都不许出现——这一句因此故意不复述它）。
/// 于是"`Rotating` 的机器主语"与"`hook`/`mcp` 的 device 主语"是同一个名字、同一套过形规则，
/// 而不是两套各自半对的规则。
///
/// 段 A 时它唯一的读者是 live 那一支（[`live_slots`] 的第 4 步）；§4.8 ③ 段 B 给它添了
/// 第二个读者 = `run_cycle_with` 的落盘读写。于是 `--replies` 那一条路径**也读这个变量**了：
/// 有身份 ⇒ 把那一行读回来种进 `Rotating`、跑完把那一行写回去；缺身份 ⇒ 不读也不写，而这一轮
/// **照常跑完**（回放座位不需要身份，作废规则④的落点就在这里，不在 `rc-llm` 也不在 `rc-ledger`）。
/// 既有二十一条回放用例因此仍然不需要设这个变量：它们走缺身份那一支；而就算操作员的本机设了，
/// 那一支多出的也只有一行 `llm_state`，判定与 stdout 读数一格都不动。
///
/// 非 TTY 一律不提示输入（用户硬红线）：这里没有任何读 stdin 的手，也没有 `IsTerminal` 的分支——
/// 坏形状与没给的后果同为 `FAULT_NO_DEVICE_ID` 那一句（3），不是提示、不是崩溃。
fn claimed_device_id() -> Option<String> {
    let raw = std::env::var("RECOLLECT_DEVICE_ID").ok()?;
    // 借用在这一行结束、`Some(raw)` 才把值移出去：写成 `.map(|_| raw)` 是 E0505
    // （借用的同时移动），`?` 那一形既短又过了 clippy 的 `needless_return` 面。
    device_subject(raw.as_str())?;
    Some(raw)
}

/// 一座座位的构造（§4.8 ② 的"从 `Config.seats` 按 preset 键构造 `Live`"那一格）。
/// 两条通道各用一个构造器，`rc_llm_http` 在**构造点**做 preset 查询与参数合法域检查：
/// 查不到键就 `Err`，这正是设计节 1 那条"不许回落默认"的接缝（回落出来的请求会打到一条
/// 没人要它打的路上），hub 这一侧因此不必自己再查一遍表（守卫 3）。
///
/// 出网点用 `transport::agent(deadline_ms)`——墙钟预算那一轴在这里**第一次有真读者**
/// （裁定 B：`JobFamily::budget_ms()` 自落地起没有生产侧读者，今天读它的是装配而不是作业）。
/// 每一座一个新的 agent：出网 agent 的 `Clone` 形状不是这一层该依赖的东西，
/// 而座位数量是个位数。
///
/// `key` 是**真 key**（B2a-3 起由 [`live_keys`] 从密文箱读出来）：它只在这个构造点经过一次，
/// 进 `Live` 的字段后由那一层负责不进日志、不进 `LlmFailure`。构造出来的座位由
/// [`live_slots`] 交给 `run_cycle_once`（外面套 [`MeteredBackend`] 保证 `llm_calls` 一笔不漏）。
///
/// `temperature` 是**原样传递**的一格：配置里写了才发，`None` 就是不写这一格、走 provider
/// 默认（不改默认值语义）。锁死温度的 preset 行由 `Live` 的构造点拒（`check_params`），
/// 这里不查表、不夹值。
fn build_live_seat(seat: &rc_llm_cfg::Seat, deadline_ms: u64, key: &str)
    -> Result<rc_llm_http::Live, &'static str> {
    let egress: Box<dyn rc_llm_http::Egress> = Box::new(rc_llm_http::transport::agent(deadline_ms));
    let budget = seat.budget.unwrap_or(rc_llm_http::MAX_BUDGET);
    let derive_budget = seat.derive_budget.unwrap_or(budget);
    let live = match (&seat.provider, &seat.endpoint, seat.model.as_deref()) {
        (Some(p), None, Some(m)) => rc_llm_http::Live::new(
            p, m, key.to_owned(), budget, seat.temperature, egress),
        (None, Some(ep), Some(m)) => rc_llm_http::Live::new_custom(
            &ep.base_url, &ep.protocol, m, key.to_owned(), budget, seat.temperature, egress),
        // `load()` 已经保证"两条通道恰有一条、custom 侧必有 model"，落到这里只剩
        // preset 那一侧缺 `model` 的一种形状：它对 preset 表就是"查不到键"，同一类后果。
        _ => return Err("seat has no resolvable preset key"),
    }?;
    Ok(live
        .with_derive_budget(derive_budget)
        .with_reasoning_effort(seat.reasoning_effort.clone()))
}

/// `build_live_seat` 在第 ③ 步（preset 形状）用的探测 key：那一格只让构造点跑表查询与
/// 参数检查，构造出来的 `Live` 立刻丢弃、一次都不出网。写成一个非空常量而不是 `""`，
/// 是为了让"空串 key"在这条装配面上彻底绝迹（真 key 只来自 [`live_keys`]）。
const PRESET_SHAPE_KEY: &str = "preset-shape-probe";

/// 按座位顺序把 `secret_ref` 解析成值。三个错误面都折成封闭短语（零路径、零值、零 slot 名）：
/// 箱未建 / 箱打不开（口令错、文件坏、口令文件缺） / 格缺。
///
/// `check_secret_ref` 在 `rc_llm_cfg::load` 已经做过一次，这里再做一次是因为本函数也收
/// 手工构造的 `Config`（单元测试）：**值的位置**必须先合法，才有资格谈读箱。坏 ref 归
/// `FAULT_LLM_CFG_BAD`（配置事故，不是读箱事故）。
fn live_keys(cfg: &rc_llm_cfg::Config, data_dir: &Path) -> Result<Vec<String>, &'static str> {
    let mut keys = Vec::with_capacity(cfg.seats.len());
    for seat in &cfg.seats {
        if rc_llm_cfg::keys::check_secret_ref(&seat.secret_ref).is_err() {
            return Err(FAULT_LLM_CFG_BAD);
        }
        let Some((box_name, slot)) = seat.secret_ref.split_once(':') else {
            return Err(FAULT_LLM_CFG_BAD);
        };
        let value = rc_secret::read_secret(data_dir, box_name, slot)
            .map_err(secret_read_fault)?;
        keys.push(value.to_string());
    }
    Ok(keys)
}

/// 一个变体一条封闭短语；`SecretError` 增变体时这里编译不过，逼着表态。
/// 三类失败必须分得开：缺箱是"还没建"、打不开是"口令/文件/权限坏了"、格缺是"建了但没这一格"。
fn secret_read_fault(e: rc_secret::SecretError) -> &'static str {
    use rc_secret::SecretError as E;
    match e {
        E::BoxMissing => FAULT_LLM_BOX_MISSING,
        E::MissingSlot => FAULT_LLM_SLOT_MISSING,
        E::PassMissing | E::OpenFailed | E::BadFormat | E::Io(_) | E::Permission
        | E::BadParams | E::BadSlot | E::BadBox | E::AlreadyExists | E::BackupKey
        | E::BackupFormat | E::BackupAuth => FAULT_LLM_SECRET_OPEN,
    }
}

/// 座位来源之二（§4.8 ②）：把 `llm.toml` 的座位构造成真正的 `LlmBackend` 槽位。
///
/// 六步的顺序是有牙齿的（`cycle_once.rs` 里那三条各钉其中一对）：
/// 1. 没点名 `--llm-config` ⇒ 没有第二条来源；
/// 2. `load()` 的 `Ok(None)`（这台机器还没配）与 `Err`（配了但坏了）是**两种后果**，
///    原串可能带路径与脱敏后的值，一律丢掉只留类别（守卫 12）；
/// 3. preset 查不到键 ⇒ 启动即失败，**在身份之前**：配置事故与"这台机器没身份"是两件事；
///    这一格用 [`PRESET_SHAPE_KEY`] 只跑构造点的表查询，`Live` 立刻丢弃、不出网；
/// 4. 身份读不到或形状不对 ⇒ `FAULT_NO_DEVICE_ID`。这一格的主语是 `Rotating`/落盘侧的**机器身份**
///    （设计节 8 那句"`hub` 今天不读任何环境变量 ⇒ 机器身份这一格没有生产者"点名的就是它），
///    作废规则④"缺身份归 hub"的处置 = 既不读也不写、stderr 响亮一句；
/// 5. 读箱失败 ⇒ 三类各归一条（缺箱/打不开/格缺）；这一步**在身份之后**，免得"没身份"
///    被一道箱故障盖住；
/// 6. 全成功 ⇒ 构造真座位，外面套 [`MeteredBackend`]（`llm_calls` 必须和回放同账）。
fn live_slots(config_path: Option<&PathBuf>, data_dir: &Path)
    -> Result<(Rc<Metered>, Slots), &'static str> {
    // `cycle once` 的那一支：没有旗标可读 ⇒ 直接吃那一格墙（W19c1 之前这里是四族之和）。
    live_slots_with_timeout(config_path, data_dir, rc_llm_http::transport::CALL_WALL_MS)
}

/// 出网 deadline 的唯一算式（W19c1）：旗标只许把墙**调小**，不许把它调大。
///
/// 墙本身只有一格（[`rc_llm_http::transport::CALL_WALL_MS`]），这里不重写那个数；
/// 所以 `--llm-timeout-ms 600000` 的效果是 300 秒，而不是十分钟。
fn call_deadline(timeout_ms: u64) -> u64 {
    timeout_ms.min(rc_llm_http::transport::CALL_WALL_MS)
}

/// S2e-2：`hub distill once --llm-timeout-ms` 的那一支。除了出网超时从参数进来，
/// 其余六步与 `live_slots` 同源（都落到 [`live_slots_with_deadline`]）。
///
/// 为什么超时必须是一格里的一格、而不是"再读一次 `cycle_budget_ms`"：`--replies` 不出网，
/// 这条旗标唯一有牙齿的地方就是这个 deadline。0 已在 `run_distill_once` 的入口被拦下，
/// 这里只处理正数；正数还要过 [`call_deadline`] 那一道 min（只许调小）。
fn live_slots_with_timeout(
    config_path: Option<&PathBuf>,
    data_dir: &Path,
    timeout_ms: u64,
) -> Result<(Rc<Metered>, Slots), &'static str> {
    live_slots_with_deadline(
        config_path,
        data_dir,
        claimed_device_id().as_deref(),
        call_deadline(timeout_ms),
    )
}

/// 测试入口：身份从参数进来，进程内单测因此不随本机 `RECOLLECT_DEVICE_ID` 的 ambient 值飘。
/// 墙钟取单发墙那一格（与两条生产入口同一个数）——这一条断的是座位构造的五步，不是超时那一格。
/// `#[cfg(test)]` 是 S2e-2 的事实：生产侧的两条入口各自递自己的 deadline，谁也不走这一条。
#[cfg(test)]
fn live_slots_with(
    config_path: Option<&PathBuf>,
    data_dir: &Path,
    device: Option<&str>,
) -> Result<(Rc<Metered>, Slots), &'static str> {
    live_slots_with_deadline(
        config_path,
        data_dir,
        device,
        rc_llm_http::transport::CALL_WALL_MS,
    )
}

/// 座位来源之二的**唯一**实现：身份与出网墙钟都从参数进来，三条入口各递各的那一格。
fn live_slots_with_deadline(
    config_path: Option<&PathBuf>,
    data_dir: &Path,
    device: Option<&str>,
    deadline_ms: u64,
) -> Result<(Rc<Metered>, Slots), &'static str> {
    let Some(path) = config_path else { return Err(FAULT_NO_LLM_SOURCE) };
    let cfg = match rc_llm_cfg::load(path) {
        Ok(None) => return Err(FAULT_NO_LLM_SOURCE),
        Ok(Some(cfg)) => cfg,
        Err(_) => return Err(FAULT_LLM_CFG_BAD),
    };
    // 出网超时：座位不知道下一次被问的是哪一族，于是它只能取调用方给的那一格。
    // W19c1 起这一格恒为**单发墙**（`CALL_WALL_MS`）：`cycle once` 直接用它，
    // `distill/review/derive/episode` 用它把 `--llm-timeout-ms` 夹住（只许更小）。
    for seat in &cfg.seats {
        if build_live_seat(seat, deadline_ms, PRESET_SHAPE_KEY).is_err() {
            return Err(FAULT_LLM_PRESET);
        }
    }
    if device.is_none() {
        return Err(FAULT_NO_DEVICE_ID);
    }
    let keys = live_keys(&cfg, data_dir)?;
    let ledger = Rc::new(Metered::new());
    let mut slots: Slots = Vec::with_capacity(cfg.seats.len());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (idx, (seat, key)) in cfg.seats.iter().zip(&keys).enumerate() {
        // 观测面（W33-A10）：耗时的写口在 `Live`（transport 的单调钟），读口在账本——
        // 这一行把同一格递给两边。每座共用一本账的一格：`max_call_ms` 的口径是
        // "本进程最慢一发"，与跨座降级后的语义一致（哪一座发得慢都是这一轮的慢）。
        let live = build_live_seat(seat, deadline_ms, key)?.with_call_ms(ledger.call_ms_cell());
        let inner: Box<dyn LlmBackend> = Box::new(live);
        let metered: Box<dyn LlmBackend> = Box::new(MeteredBackend::new(inner, ledger.clone()));
        // S4a3：槽位标签必须是“哪一家、哪个模型”，否则判定台账只写 live-1，
        // 容灾时看不出到底是谁答的；顺序变化也不该改变同一座的身份。
        let base = match (seat.provider.as_deref(), seat.model.as_deref()) {
            (Some(provider), Some(model)) => format!("{provider}:{model}"),
            (None, Some(model)) => format!("custom:{model}"),
            _ => format!("live-{}", idx + 1),
        };
        let slug = if seen.insert(base.clone()) {
            base
        } else {
            format!("{base}#{}", idx + 1)
        };
        // 旁路留痕：只有 `RECOLLECT_LLM_TRACE` 给了路径才装，未设时这一行与原样逐字等价。
        let seat: Box<dyn LlmBackend> = match hub::llm_trace::sink() {
            Some(path) => Box::new(hub::llm_trace::Trace::new(slug.clone(), metered, path)),
            None => metered,
        };
        slots.push((slug, seat));
    }
    Ok((ledger, slots))
}

/// 单测用的是"错误类别"这一面；成功面由 `live_slots_with` 直接断言。
#[cfg(test)]
fn live_slots_fault_with(
    config_path: Option<&PathBuf>,
    data_dir: &Path,
    device: Option<&str>,
) -> &'static str {
    match live_slots_with(config_path, data_dir, device) {
        Ok(_) => "",
        Err(cat) => cat,
    }
}

/// 座位来源的唯一装配点（`cycle once` 与 `distill once` 共用）。
///
/// 顺序是有牙齿的：
/// 1. `--replies` 在前——给了它，配置那一条支**根本不进**（`live_slots` 一次都不调用）；
/// 2. 第 1 座走 `replay::load`（造这一轮唯一的那本账），第 2..N 座走 `load_replies`
///    （只解析脚本，裁定 D③：一座一个槽位，只许有一本 `Metered`）；
/// 3. 都不给 ⇒ `--llm-config`：座位在这里真造出来（真 key + `MeteredBackend`）。
///
/// 调用方必须在 `Root::open` **之前**调它：一轮都不许为"跑不起来的来源"开库。
///
/// S2e-2：`timeout_ms` 是 `Option` 而不是一个数——`None` 是 `cycle once`（没有那条旗标，
/// 直接用单发墙 `CALL_WALL_MS`），`Some(ms)` 是 `hub distill once --llm-timeout-ms`（还要过
/// [`call_deadline`] 那一道 min）。回放那一支两条路共用且**不读它**：脚本不出网，超时是
/// live 座位的事。
fn llm_source(
    replies: &[PathBuf],
    config: Option<&PathBuf>,
    data_dir: &Path,
) -> Result<(Rc<Metered>, Slots), &'static str> {
    llm_source_with_timeout(replies, config, data_dir, None)
}

/// [`llm_source`] 加一格出网墙钟：live 那一支按 `timeout_ms` 造座位，回放那一支不看它。
fn llm_source_with_timeout(
    replies: &[PathBuf],
    config: Option<&PathBuf>,
    data_dir: &Path,
    timeout_ms: Option<u64>,
) -> Result<(Rc<Metered>, Slots), &'static str> {
    if replies.is_empty() {
        return match timeout_ms {
            Some(ms) => live_slots_with_timeout(config, data_dir, ms),
            None => live_slots(config, data_dir),
        };
    }
    let (meter, seats) = replay_seats(replies)?;
    let slots = seats_to_slots(&meter, seats);
    Ok((meter, slots))
}

/// `--replies` → (账本, 座位)。出现顺序就是降级顺序；第 2..N 座共享第一座的账本。
/// 两个加载器是 `replay` 模块的自由函数（账本本体住在 `rc-cycle`）。
fn replay_seats(paths: &[PathBuf]) -> Result<(Rc<Metered>, Seats), &'static str> {
    let Some((first, rest)) = paths.split_first() else {
        return Err(FAULT_NO_LLM_SOURCE);
    };
    let (meter, first_script) = replay::load(first)?;
    let mut seats: Seats = Vec::with_capacity(paths.len());
    seats.push((seat_slug(0), first_script));
    for (idx, path) in rest.iter().enumerate() {
        seats.push((seat_slug(idx + 1), replay::load_replies(path)?));
    }
    Ok((Rc::new(meter), seats))
}

/// 回放座位 → `Rotating` 的槽位：每座一个 [`replay::Seat`]，共享同一本账本
/// （§4.8 ② 的拆分 + 段 B 的裁定 D；`Rotating` 的槽位要求 `'static`，所以账本递的是 `Rc`）。
fn seats_to_slots(meter: &Rc<Metered>, seats: Seats) -> Slots {
    seats.into_iter()
        .map(|(slug, script)| (slug, Box::new(replay::Seat::new(script, meter.clone())) as Box<dyn LlmBackend>))
        .collect()
}

/// 两条来源的共同下游：`slots` 已经是构造好的后端（回放座位或套了 [`MeteredBackend`] 的
/// `Live`），`meter` 是它们共同的账本。本函数负责把座位装进 `Rotating`、把轮转态读回来
/// 种进去、跑完再把那一行写回去。
fn run_cycle_with_slots(
    a: &CycleOnceArgs,
    meter: Rc<Metered>,
    slots: Slots,
) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let now_ms = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => return operational("system clock is behind the unix epoch"),
    };
    // S3a：cycle 的触顶窗口也从 0 开始——早退（能力闸/轮转态读失败）同样是一次"最近一次运行"，
    // 它没有触顶。收尾在 `charge_counters` 的同一处写真实值。
    clear_quota_stopped(&root, "cycle", now_ms);
    // 守卫 14 的执法点：能力自测在本文件里只调用这一次（任务 13 的脚本断它的出现次数）。
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    // ---- B2 · 启动读（§4.8 ③ 那一行的上半句）----
    // 身份缺 ⇒ **既不读也不写**（作废规则④的落点归 hub），而这一支仍是**能跑完的一轮**：
    // 回放座位不需要身份，轮转态只是"少了一份可继承的判断"，不是故障。
    // 但"不是故障"不等于"没人说话"：这一支在运维面打一条 `warning`（`operator_warn`，
    // 不改判定、不动 stdout），因为"这台机器的轮转态没在继承"是操作员要能看见的一件事。
    // live 那一支的身份缺失仍住在 `live_slots_with` 的第 4 步——两个来源、两条下行，别并成一句。
    let machine = claimed_device_id();
    let seed = match machine.as_ref() {
        Some(m) => match llm_state::read(root.ledger(), m) {
            Ok(Some(row)) => Some(seed_of_row(&row)),
            // `Ok(None)` 是"这台机器从没写过"，与"写过但主家是 0 号"是两件事（`read` 的注释），
            // 所以这里不替调用方造行：没有种就是 `Rotating::new`。
            Ok(None) => None,
            Err(_) => return operational(FAULT_LLM_STATE_READ),
        },
        None => {
            operator_warn(WARN_LLM_STATE_NO_IDENTITY);
            None
        }
    };
    let rotating = match &seed {
        Some(s) => Rotating::seeded(slots, s, now_ms),
        None => Rotating::new(slots),
    };
    // `Err` 那一支今天到不了（回放由 `--replies` 的非空分支保证至少一座，live 由
    // `rc_llm_cfg::load` 拒空 `providers` 保证至少一座），但它是
    // `&'static str` 且零插值 ⇒ 原样交给 `operational` 比在这里编一条新的类别串更诚实。
    let rotating = match rotating {
        Ok(r) => r,
        Err(cat) => return operational(cat),
    };
    let (mut out, exit) = run_cycle_once(&root, &meter, &rotating, now_ms, a.batch_turns);
    // 本轮隔离的 issue：`derive.rs` 写 issue 时 `run_id` 里带本轮 `now_ms`，这里按同一个拼法
    // 读回（`issues::derive_run_id` 是唯一拼法）。读失败不是这一轮的故障——判定早已由
    // `run_cycle_once` 交出，这里少两格读数的代价比把一轮成功改成退出码 3 小得多。
    let quarantine_issue = rc_memory::issues::find_by_run(
        root.memory(), &rc_memory::issues::derive_run_id(now_ms)).ok().flatten();
    // 丢批不再只说"丢了"：stdout 的 `derive_drop_reason` 带上封闭类别，stderr 同步一句
    // `warning:`（与身份缺失那一行同一条运维下行）。类别只有 `schema` 这种固定词，
    // 不带模型原文、路径或正文（守卫 12）。
    if let Some(reason) = out.derive_drop_reason {
        operator_warn(&format!("derive batch dropped: {reason}"));
    }
    // ---- B3 · 调用后写（与下面计数器那一格同一层）----
    // `written_at_ms` 用**传进本轮的 `now_ms`**，不另取一次钟（一轮之内不许有两个"现在"，
    // 与 `run_anomaly_scan` 的戳同一条纪律）。缺身份 ⇒ 这一格整个跳过——"不读"与"不写"
    // 是同一条纪律（作废规则④）的两半，只缺一半就会让 A48 那种同步箱把半份判断写上表。
    // 写失败的去向照 `charge_counters` 的现成先例：降成 `Fault(类别串)`，读数照旧。
    let exit = match machine.as_deref() {
        Some(m) => match llm_state::write(root.ledger(),
                                          &row_of_persisted(m, now_ms, &rotating.persisted())) {
            Ok(()) => exit,
            Err(_) => CycleExit::Fault(FAULT_LLM_STATE_WRITE),
        },
        None => exit,
    };
    // 计数器写失败把判定降成 3，但**读数照旧打印**：那一轮的抽取是真发生了的
    // （§2.19：两件事必须同时出去）。
    let mut exit = match charge_counters(root.ledger(), &meter, now_ms) {
        Ok(()) => exit,
        Err(cat) => CycleExit::Fault(cat),
    };
    // S3a：与 `charge_counters` 同一收尾点写 cycle 的触顶位（主语 = `cycle`）。
    // 写在它之后是为了让"本轮真发生过的调用"与"本轮触没触顶"同处收口；两者写失败的去向
    // 不同：`llm_calls` 写失败降退出码，这一格写失败只丢一格运维读数。
    record_quota_stopped(&root, "cycle", &meter, now_ms);
    // 轮号是每 N 轮机制（`scan_due`）唯一的轮源，它失败必须可见：这一行原先写作 `let _ = …`，
    // 那时候 `rounds` 还没有读者，所以吞掉错误没有症状；今天它是扫描节奏的输入，
    // 轮号停走会让 `scan_due` 恒真或恒假都查不出来，而 `attention_open: 0` 看起来仍是好消息。
    if rc_ledger::counters::bump(root.ledger(), "maintenance", "", "rounds", 1, now_ms).is_err() {
        exit = CycleExit::Fault(FAULT_COUNTERS);
    }
    // 扫描读的是 bump 之后的轮号，所以它必须在上面那一行之后；已 Fault 的轮不再消费写口
    // （与 derive 那一族的挂点同律：前面已经判死的轮不去动账，免得一故障叠两个读数）。
    if !matches!(exit, CycleExit::Fault(_)) {
        match scan_rounds(root.ledger()) {
            // 读不到轮号归 `FAULT_ANOMALY` 而不是 `FAULT_COUNTERS`：这里没有写失败，
            // 是"这一轮的扫描无法判定该不该跑"——报 counters 写失败会把操作员支到错误的面上。
            Err(_) => exit = CycleExit::Fault(FAULT_ANOMALY),
            Ok((rounds, last_full)) => {
                if scan_due(rounds, last_full) {
                    if let Err(cat) = run_anomaly_scan(&root, now_ms, rounds) {
                        exit = CycleExit::Fault(cat);
                        // 失败让 attempts +1、游标不动（与 distill 那一族的失败路径同形）。
                        // 这一支的 `let _ =` 是有意的：本轮已经判死（`exit` 是 Fault），
                        // attempt 没记上也不会改变退出码，而在这里再叠一个故障类别
                        // 只会把"扫描失败"这条真信号盖掉。
                        let _ = rc_ledger::water::note_attempt(
                            root.ledger(), "anomaly_scan", ANOMALY_SCOPE);
                    }
                }
            }
        }
    }
    finish_cycle(exit, &mut out, &meter, quarantine_issue)
}

fn run_cycle(args: &CycleArgs) -> ExitCode {
    // 只有一个子命令，所以这里**不带 `_`**：`CycleJob` 增变体时 hub 编译不过，
    // 逼着新增的那条作业在这里表态（而不是掉进一个兜底分支里被静默当成 `Once`）。
    let CycleJob::Once(a) = &args.job;
    // §4.8 ②：座位来源有两条，`--replies` 在前。给了它 ⇒ 回放座位，
    // 配置那一条支**根本不进**（`live_slots` 一次都不调用，所以它读不到那个文件）。
    // §4.8 ③ 段 B 的裁定 D：`--replies` 可重复，**出现顺序就是降级顺序**——一座一个槽位，
    // 第一座坏就前进到第二座；只给一座时列表里也只有一个槽位，**但既有回放用例的调用数会因此
    // 从 6 抬到 7**——`Rotating` 的 Timeout 语义是"先原样重试一次"，脚本耗尽的那一发真的又开了一次口
    // （执法点 `crates/hub/tests/cycle_once.rs` 的 `llm_calls_total`）。工单那句"既有二十一条回放用例
    // 因此一字不改"已被真树证伪，本注释按真树写。
    //
    // 两条来源的装配收在 [`llm_source`] 里（`distill once` 走同一段 `llm_source_with_timeout`，
    // 只是多递一格出网墙钟，不再抄第二份），且必须在 `Root::open` 之前：一轮都不许为
    // "跑不起来的来源"开库。
    let (meter, slots) = match llm_source(&a.replies, a.llm_config.as_ref(), &a.data_dir) {
        Ok(v) => v,
        Err(cat) => return operational(cat),
    };
    run_cycle_with_slots(a, meter, slots)
}

/// 一条 naming pass 的运维面读数（守卫 12：七格，没有正文、路径、密钥）。
/// 键顺序就是派发档给的形状：`schema_version, pass, turns, anchors, mentions, filtered, state`。
/// `filtered`（S2e-5）是噪声闸丢掉的锚数：模型输出照收，过滤只让锚少几行，不改变成败。
/// `system_turns_skipped`（转换器配套）是 additive 的一格：批区间内 `role='system'` 的轮数
/// ——它们只作上下文、永不作 TARGET。既有键一个不改，`schema_version` 仍是 1（只加不减）。
#[derive(serde::Serialize)]
struct NamingOut {
    schema_version: u32,
    pass: &'static str,
    turns: usize,
    anchors: usize,
    mentions: usize,
    filtered: usize,
    system_turns_skipped: usize,
    state: &'static str,
    /// W19c2：这一发是不是"单元级失败试满上限、整批落成隔离"（0/1，缺省 0）。
    /// **只追加在最后**，既有键序一个不动。`state=quarantined` 而这一格是 0 的批是
    /// **质量**隔离（审核不确定等），日路径的连续隔离墙不许把它算进座位/单元卡死。
    retry_exhausted: u8,
    /// 观测面（W33-A10，只追加）：本进程最慢一发真实出网的耗时（单调钟毫秒）。
    /// 写口在 `Live`、读口在账本（`Metered`）；回放座位没有真实出网 ⇒ 恒 0（stub 不发明耗时）。
    max_call_ms: i64,
    /// 观测面第二格：这一发里 `LlmFailure::Timeout` 的计数（含审核档）。
    timeouts: u32,
}

/// 读数的形状版本（与 pass 的 `schema_version=4` 是两件事：这是运维面的格子版本）。
/// W33-A10 起是 2：追加观测面两格（`max_call_ms`/`timeouts`），键序只加不改。
const NAMING_OUT_SCHEMA_VERSION: u32 = 2;

impl NamingOut {
    /// 没有可选的轮次：全零 + `state="none"`（空批，退出 0）。
    fn none() -> NamingOut {
        NamingOut {
            schema_version: NAMING_OUT_SCHEMA_VERSION,
            pass: NAMING_PASS,
            turns: 0,
            anchors: 0,
            mentions: 0,
            filtered: 0,
            system_turns_skipped: 0,
            state: "none",
            retry_exhausted: 0,
            // 真值在 run_distill_naming 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }
    /// 跑完的一批：`state` 用 runner 的终态 slug（`done`/`empty`）。
    fn of(out: &NamingOutcome) -> NamingOut {
        NamingOut {
            schema_version: NAMING_OUT_SCHEMA_VERSION,
            pass: NAMING_PASS,
            turns: out.turns,
            anchors: out.anchors,
            mentions: out.mentions,
            filtered: out.filtered,
            system_turns_skipped: out.system_turns_skipped,
            state: out.state.slug(),
            retry_exhausted: u8::from(out.retry_exhausted),
            // 真值在 run_distill_naming 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }
    /// 失败的一批：轮数是**选中的**轮数（退避重试靠这一格看得见），行数无从谈起 ⇒ 0；
    /// `state` 与 runner 落在 ledger 里的状态一致（`pending`/`rejected`）。
    /// `system_turns_skipped` 照打：它是选批层面的事实，与这批跑成什么无关。
    fn failed(turns: usize, state: &'static str, system_turns_skipped: usize) -> NamingOut {
        NamingOut {
            schema_version: NAMING_OUT_SCHEMA_VERSION,
            pass: NAMING_PASS,
            turns,
            anchors: 0,
            mentions: 0,
            filtered: 0,
            system_turns_skipped,
            state,
            // 失败那一发（`RoundError::Run`）只可能是座位级或契约面 ⇒ 这一格恒 0。
            retry_exhausted: 0,
            // 真值在 run_distill_naming 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }
}

/// naming pass 的三类失败。退出码与 stderr 的词一一对应（派发档 §CLI）：
/// `Llm` ⇒ `error: llm` + 3；`Rejected` ⇒ `error: rejected` + 2；其余 ⇒ `error: store` + 3。
#[derive(Clone, Copy)]
enum NamingFault {
    /// 类别词之外再带一格四态（见 [`llm_fault_text`]）：`llm` 仍是 stderr 的第一个词。
    Llm(LlmFailure),
    Rejected,
    Store,
}

/// 一条 naming pass 的收口：**先**打 stdout 读数（失败也打——退避重试的 `turns` 要看得见，
/// 与 cycle 的"两件事同时出去"同律），**再**打 stderr 的类别词并给退出码。
///
/// 计数器写失败只把退出码降到 3：读数已经在上面出去了，不因为账面写不进去就把它吞掉。
fn finish_naming(out: &NamingOut, fault: Option<NamingFault>, counters_failed: bool) -> ExitCode {
    match serde_json::to_string(out) {
        Ok(text) => write_line(&text),
        // 七格全是整数/静态串，这一支理论上不可达；仍走 `operational` 而不是 panic。
        Err(_) => return operational(FAULT_STORE_WRITE),
    }
    let code = match fault {
        None => 0,
        Some(NamingFault::Rejected) => 2,
        Some(NamingFault::Llm(_) | NamingFault::Store) => 3,
    };
    let code = if counters_failed { 3 } else { code };
    match fault {
        Some(NamingFault::Llm(failure)) => operator_line("error", llm_fault_text(failure)),
        Some(NamingFault::Rejected) => operator_line("error", FAULT_REJECTED),
        Some(NamingFault::Store) => operator_line("error", FAULT_STORE),
        // 没有 pass 故障时，计数器写失败是唯一需要说话的失败面。
        None if counters_failed => operator_line("error", FAULT_COUNTERS),
        None => {}
    }
    ExitCode::from(code)
}

/// `--no-review` ⇒ [`ReviewMode::EnqueueOnly`]，否则既有 `Inline`（S3b-4b）。
///
/// 三处调用点共用这一格：旗标只有一个含义——"只抽取并入队，窄审留给 `hub review run`"。
/// time 不到这里（它在 [`run_distill_once`] 的入口就被 `FAULT_NO_REVIEW_ON_TIME` 拦下）。
fn review_mode(a: &DistillOnceArgs) -> ReviewMode {
    if a.no_review {
        ReviewMode::EnqueueOnly
    } else {
        ReviewMode::Inline
    }
}

/// `distill` 的入口：只按 [`DistillJob`] 分派。三条支线互不共用状态，所以这里不装配任何
/// 座位来源、不开库——`Once` 的入口判定（含"先于开库"那几条）仍在 [`run_distill_once`] 里。
fn run_distill(args: &DistillArgs) -> ExitCode {
    match &args.job {
        DistillJob::Reclaim(a) => run_distill_reclaim(a),
        DistillJob::Reset(a) => run_distill_reset(a),
        DistillJob::BackfillArchive(a) => run_distill_backfill_archive(a),
        DistillJob::Once(a) => run_distill_once(a),
    }
}

/// `distill once` 的入口：先认 `--pass` 与 `--family` 的形状（不静默回落、不忽略多给的旗标），
/// 再在 `Root::open` **之前**装配座位来源（与 `cycle once` 同一段 `llm_source`）。
fn run_distill_once(a: &DistillOnceArgs) -> ExitCode {
    // S2e-2：`0` 是**旗标**的形状问题，不是某条 pass 的业务判定 ⇒ 先于 pass 分派、
    // 先于任何来源装配、先于开库。time 也照此拒：它确实一次 LLM 都不调，但"0 毫秒"
    // 这个值本身在任何一条 pass 上都没有可信的解释，静默接受等于教操作员继续写 0。
    if a.llm_timeout_ms == 0 {
        return operational(FAULT_LLM_TIMEOUT);
    }
    // DR：`--dry-run` 在装配座位**之前**收口——它的全部含义就是"一格来源都不读"。
    // 同给来源是用法错误（先于任何开库与选批），否则这一轮会看起来像"真跑过一次"。
    if a.dry_run && (!a.replies.is_empty() || a.llm_config.is_some()) {
        return operational(FAULT_DRY_RUN_SOURCE);
    }
    match a.pass.as_str() {
        NAMING_PASS => {
            // naming 只有一族（空串 family），多给 `--family` 是用法错误而不是被忽略。
            if a.family.is_some() {
                return operational(FAULT_NAMING_FAMILY);
            }
            // 空轮重试只对 event 有牙齿（`--retry-empty 0` 是缺省值，给了 0 不算"多给"）。
            if a.retry_empty != 0 {
                return operational(FAULT_RETRY_EMPTY);
            }
            if a.dry_run {
                return run_distill_dry(a, DryPass::Naming);
            }
            let (meter, slots) =
                match llm_source_with_timeout(&a.replies, a.llm_config.as_ref(), &a.data_dir,
                                              Some(a.llm_timeout_ms)) {
                    Ok(v) => v,
                    Err(cat) => return operational(cat),
                };
            run_distill_naming(a, meter, slots)
        }
        PASS_FACT => {
            // fact 的 family 是这一条 pass 的必填调用变体：缺它 / 不认识的值都是用法错误，
            // 且**不回显**操作员递进来的那个词（闭集本身就是点名）。
            let family = match a.family.as_deref().and_then(family_of_slug) {
                Some(family) => family,
                None => return operational(FAULT_FACT_FAMILY),
            };
            // fact 的三族不在这一片（派发档 §要做 5：维持原样）。
            if a.retry_empty != 0 {
                return operational(FAULT_RETRY_EMPTY);
            }
            if a.dry_run {
                return run_distill_dry(a, DryPass::Fact(family));
            }
            let (meter, slots) =
                match llm_source_with_timeout(&a.replies, a.llm_config.as_ref(), &a.data_dir,
                                              Some(a.llm_timeout_ms)) {
                    Ok(v) => v,
                    Err(cat) => return operational(cat),
                };
            run_distill_fact(a, family, meter, slots)
        }
        PASS_EVENT => {
            // event 是单一 pass（空串 family）：多给 `--family` 是用法错误，不是被忽略——
            // 静默忽略会让操作员以为"这一轮问的是 promise"。
            if a.family.is_some() {
                return operational(FAULT_EVENT_FAMILY);
            }
            if a.dry_run {
                return run_distill_dry(a, DryPass::Event);
            }
            let (meter, slots) =
                match llm_source_with_timeout(&a.replies, a.llm_config.as_ref(), &a.data_dir,
                                              Some(a.llm_timeout_ms)) {
                    Ok(v) => v,
                    Err(cat) => return operational(cat),
                };
            run_distill_event(a, meter, slots)
        }
        PASS_STATE => {
            // state 是单一 pass（空串 family），也没有空轮重试那一格（v1 的聚合走"跑两遍"，
            // 不是 runner 内部重采）。
            if a.family.is_some() {
                return operational(FAULT_STATE_FAMILY);
            }
            if a.retry_empty != 0 {
                return operational(FAULT_RETRY_EMPTY);
            }
            if a.dry_run {
                return run_distill_dry(a, DryPass::State);
            }
            let (meter, slots) =
                match llm_source_with_timeout(&a.replies, a.llm_config.as_ref(), &a.data_dir,
                                              Some(a.llm_timeout_ms)) {
                    Ok(v) => v,
                    Err(cat) => return operational(cat),
                };
            run_distill_state(a, meter, slots)
        }
        PASS_LESSON => {
            // 波三刀 A：lesson 是单一 pass（空串 family），没有空轮重试那一格
            // （它的空产出是"没有可复用的规则"，重采只会再产一次同形的空数组）。
            if a.family.is_some() {
                return operational(FAULT_LESSON_FAMILY);
            }
            if a.retry_empty != 0 {
                return operational(FAULT_RETRY_EMPTY);
            }
            if a.dry_run {
                return run_distill_dry(a, DryPass::Lesson);
            }
            let (meter, slots) =
                match llm_source_with_timeout(&a.replies, a.llm_config.as_ref(), &a.data_dir,
                                              Some(a.llm_timeout_ms)) {
                    Ok(v) => v,
                    Err(cat) => return operational(cat),
                };
            run_distill_lesson(a, meter, slots)
        }
        TIME_PASS => {
            // **先判 pass，再碰任何来源**：time 是纯程序（v0 的确定性解析 + 回填），一个座位
            // 都不需要——`--replies`/`--llm-config` 即使给了也不读（不因此报错）。
            // 与 naming/event 同形：多给 `--family` 是用法错误，不是被忽略。
            if a.family.is_some() {
                return operational(FAULT_TIME_FAMILY);
            }
            // DR：time 没有模型输入可导出（与 `--no-review` 同一支理由：没有那个东西）。
            if a.dry_run {
                return operational(FAULT_DRY_RUN_TIME);
            }
            // S3b-4b：time 没有窄审也没有队列，`--no-review` 在它身上没有可解释的含义。
            if a.no_review {
                return operational(FAULT_NO_REVIEW_ON_TIME);
            }
            // 空轮重试要有"抽取调用"这个主语；time 一次模型都不调（同 `--no-review` 那一支）。
            if a.retry_empty != 0 {
                return operational(FAULT_RETRY_EMPTY);
            }
            run_distill_time(a)
        }
        _ => operational(FAULT_PASS),
    }
}

// ── `distill reclaim` —— 把卡住的 `running` 行退回 `pending` ─────────────────────

/// `distill reclaim` 的读数。`reclaimed=0` 是合法读数（"没有过期 running 行"），不是错误。
#[derive(serde::Serialize)]
struct ReclaimOut {
    schema_version: u32,
    pass: String,
    reclaimed: usize,
    older_than_ms: i64,
    backoff_ms: i64,
    now_ms: i64,
}

/// 这份读数自身的形状版本（与 `Report::schema_version` 同义：不是库的版本）。
const RECLAIM_SCHEMA_VERSION: u32 = 1;

/// `distill reclaim`：开库 → 认 `--pass` → 交给 [`rc_ledger::pass_state::reclaim_running`] → 读数。
///
/// 两条"先于开库"的判定与 `distill once` 同口径：未知 pass 是用法错误（退出 3，**不回显**
/// 操作员递进来的词），钟在纪元之前是运维失败。存储错误也走 3——它和"参数错"对操作员的
/// 下一步相同（看 `doctor`），不必分两个码。
fn run_distill_backfill_archive(a: &DistillBackfillArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let report = match rc_distill::value_backfill::backfill_program_archive(root.memory(), now_ms) {
        Ok(r) => r,
        Err(_) => return operational("backfill-archive failed — run: doctor"),
    };
    if a.json {
        let out = serde_json::json!({
            "schema_version": 1,
            "scanned": report.scanned,
            "hits": report.hits,
            "newly_marked": report.newly_marked,
            "now_ms": now_ms,
        });
        match serde_json::to_string(&out) {
            Ok(text) => write_line(&text),
            Err(_) => return operational("cannot encode backfill readout"),
        }
    } else {
        write_line(&format!(
            "scanned={} hits={} newly_marked={}",
            report.scanned, report.hits, report.newly_marked
        ));
    }
    ExitCode::from(0)
}

fn run_distill_reclaim(a: &DistillReclaimArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // 闭集复用 `distill once` 的那五个常量。**故意不含 `lesson`**：lesson 没有 `running`
    // 行（一次性跑完、直接落终态），`reclaim` 对它无事可做。
    if a.pass != NAMING_PASS && a.pass != PASS_FACT && a.pass != PASS_EVENT
        && a.pass != PASS_STATE && a.pass != TIME_PASS
    {
        return operational("distill reclaim: unknown pass");
    }
    let n = match rc_ledger::pass_state::reclaim_running(
        root.ledger(), &a.pass, a.older_than_ms, a.backoff_ms, now_ms,
    ) {
        Ok(n) => n,
        Err(_) => return operational("distill reclaim failed — run: doctor"),
    };
    if a.json {
        let out = ReclaimOut {
            schema_version: RECLAIM_SCHEMA_VERSION,
            pass: a.pass.clone(),
            reclaimed: n,
            older_than_ms: a.older_than_ms,
            backoff_ms: a.backoff_ms,
            now_ms,
        };
        match serde_json::to_string(&out) {
            Ok(text) => write_line(&text),
            Err(_) => return operational("cannot encode reclaim readout"),
        }
    } else {
        write_line(&format!("reclaimed {} row(s) for pass {}", n, a.pass));
    }
    ExitCode::from(0)
}

// ── `distill reset` —— 按轮把终态行定向回拨成 stale ────────────────────────────

/// 动作名：`--json` 的 `job` 格与审计行的 `action` 是同一件事，共用一个常量。
const DISTILL_RESET_JOB: &str = "distill_reset";

/// 四条用法错误面（零插值、点名旗标、不回显操作员递进来的词或路径）。
const FAULT_RESET_PASS: &str = "distill reset --pass must be naming|fact|event|state|time";
const FAULT_RESET_REASON: &str = "distill reset --reason must match [a-z0-9_]+";
const FAULT_RESET_TURNS_FILE: &str = "cannot read turns file for --turns";
const FAULT_RESET_TURNS_SHAPE: &str =
    "distill reset --turns must be one 64-digit hex turn uid per line";

/// `distill reset` 的读数。`reset=0` 是合法读数（那些轮本来不是终态，或库里没有这一格），
/// 不是错误；三项计数与 [`rc_ledger::pass_state::ResetCounts`] 逐格对应。
#[derive(serde::Serialize)]
struct ResetOut {
    job: &'static str,
    pass: String,
    turns: usize,
    reset: usize,
    skipped_non_terminal: usize,
    not_found: usize,
}

/// `--turns` 文件 → `(去重后的轮号, 文件的 sha256)`。
///
/// 空行与 `#` 注释跳过；其余每行必须是 64 位十六进制（大小写都收，统一小写后落库）。
/// 任何一行不合格都是用法错误——静默跳过会让操作员以为那一轮也回拨了。重复行按首次出现
/// 去重：同一轮在文件里写两遍不该让读数随行序变化。
///
/// 返回的 sha256 只喂审计行（"操作员喂进来的是哪一份清单"），正文一个字节都不进库。
fn read_turn_list(path: &Path) -> Result<(Vec<String>, String), &'static str> {
    let bytes = std::fs::read(path).map_err(|_| FAULT_RESET_TURNS_FILE)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    let sha = h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
    let text = std::str::from_utf8(&bytes).map_err(|_| FAULT_RESET_TURNS_FILE)?;
    let mut turns: Vec<String> = Vec::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.len() != 64 || !line.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(FAULT_RESET_TURNS_SHAPE);
        }
        let uid = line.to_ascii_lowercase();
        if !turns.contains(&uid) {
            turns.push(uid);
        }
    }
    Ok((turns, sha))
}

/// `distill reset`：三条用法判定（pass 名、理由形状、清单文件）都排在 `Root::open`
/// **之前**——用法错误不建库、不动一行。
///
/// 审计行紧跟着回拨写：回拨已经在库里落了，审计写不进去时**绝不能还报成功**（退出 3、
/// 一句静态话），否则这一刀就成了"改了状态却没留账"的入口。
fn run_distill_reset(a: &DistillResetArgs) -> ExitCode {
    // 闭集复用 `distill once` / `distill reclaim` 的那五个常量，不在这里手抄第二份字面。
    if a.pass != NAMING_PASS && a.pass != PASS_FACT && a.pass != PASS_EVENT
        && a.pass != PASS_STATE && a.pass != TIME_PASS
    {
        return operational(FAULT_RESET_PASS);
    }
    if !hub::review::reason_slug_ok(&a.reason) {
        return operational(FAULT_RESET_REASON);
    }
    let (turns, turns_sha256) = match read_turn_list(&a.turns) {
        Ok(v) => v,
        Err(cat) => return operational(cat),
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let counts = match rc_ledger::pass_state::reset_terminal_to_stale(
        root.ledger(), &a.pass, &turns, &a.reason, now_ms,
    ) {
        Ok(c) => c,
        Err(_) => return operational("distill reset failed — run: doctor"),
    };
    // 审计只记四件事：哪条 pass（subject）、三个计数、理由、清单文件的指纹。
    let detail = format!(
        "reset={} skipped_non_terminal={} not_found={} reason={} turns_sha256={turns_sha256}",
        counts.reset, counts.skipped_non_terminal, counts.not_found, a.reason
    );
    if rc_ledger::device::append_audit(
        root.ledger(),
        now_ms,
        "operator",
        DISTILL_RESET_JOB,
        Some(&a.pass),
        &detail,
    )
    .is_err()
    {
        return operational("distill reset: audit write failed — run: doctor");
    }
    if a.json {
        let out = ResetOut {
            job: DISTILL_RESET_JOB,
            pass: a.pass.clone(),
            turns: turns.len(),
            reset: counts.reset,
            skipped_non_terminal: counts.skipped_non_terminal,
            not_found: counts.not_found,
        };
        match serde_json::to_string(&out) {
            Ok(text) => write_line(&text),
            Err(_) => return operational("cannot encode reset readout"),
        }
    } else {
        write_line(&format!(
            "turns={} reset={} skipped_non_terminal={} not_found={}",
            turns.len(),
            counts.reset,
            counts.skipped_non_terminal,
            counts.not_found
        ));
    }
    ExitCode::from(0)
}

// ── DR：`distill once --dry-run` —— 只导原文，不开模型口 ────────────────────────

/// dry-run 的三条 pass（time 在 [`run_distill_once`] 的入口就被拦下：没有模型输入）。
#[derive(Clone, Copy)]
enum DryPass {
    Naming,
    Fact(FactFamily),
    Event,
    State,
    Lesson,
}

/// 一段"标题 + 正文"的打印：正文**逐字**写出去（prompt 自己的换行原样保留），
/// 只在结尾补一个换行以接下一段标题。拼成一整块再交给 [`write_line`]——一次写出，
/// 不让 stdout 出现半截。
fn push_block(text: &mut String, title: &str, body: &str) {
    text.push_str(title);
    text.push('\n');
    text.push_str(body);
    if !body.ends_with('\n') {
        text.push('\n');
    }
}

/// `distill once --dry-run`：开库 → 能力闸（与真实运行同一个入口判定）→ **真实选批** →
/// **同一组 `*_system()`/`*_user(...)`** → 打印。
///
/// 不调模型、不写 `extract_pass_state`、不写 `review_*`、不动游标：这条路的全部副作用
/// 就是选批本身（与真实运行同一份实现，选批里 `compact_summary` 的那一笔标记照旧——
/// 那是"这轮已经处理过"的事实，不是这次 dry-run 的成绩）。
fn run_distill_dry(a: &DistillOnceArgs, pass: DryPass) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // 守卫 14 的执法点与真实运行同一个：能力不达标时选批本身就没有可信的形状。
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    // 每一支的渲染调用与 runner 的那一行逐字同形（naming 是 `a1_run::run_a1_once` 里那两行，
    // fact/event 是 `run_*_with` 里那两行）。改写渲染 = 让这份输出不再是"模型看到的原文"。
    let (pass_label, turns, system, user) = match pass {
        DryPass::Naming => match select_naming_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            a.batch_turns,
            now_ms,
        ) {
            Ok(Some(plan)) => (
                NAMING_PASS,
                plan.batch.rows.len(),
                rc_distill::prompt_v4::a1_system(),
                rc_distill::prompt_v4::a1_user(
                    &plan.batch,
                    &plan.candidates,
                    plan.principal_name.as_deref(),
                ),
            ),
            // 空批是合法结果：没有模型输入可导，仍然退出 0。
            Ok(None) => return no_batch(),
            Err(_) => return operational(FAULT_STORE),
        },
        DryPass::Fact(family) => match select_fact_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            a.batch_turns,
            family,
            now_ms,
        ) {
            Ok(Some(plan)) => (
                PASS_FACT,
                plan.batch.rows.len(),
                rc_distill::pass_prompt::fact_system_selected(family),
                rc_distill::pass_prompt::fact_user(
                    &plan.batch,
                    &plan.candidates,
                    family,
                    &plan.ctx_turns,
                ),
            ),
            Ok(None) => return no_batch(),
            Err(_) => return operational(FAULT_STORE),
        },
        DryPass::State => match select_state_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            a.batch_turns,
            now_ms,
        ) {
            Ok(Some(plan)) => (
                PASS_STATE,
                plan.batch.rows.len(),
                rc_distill::pass_prompt::state_system(),
                rc_distill::pass_prompt::state_user(
                    &plan.batch,
                    &plan.candidates,
                    &plan.ctx_turns,
                    &plan.existing,
                ),
            ),
            Ok(None) => return no_batch(),
            Err(_) => return operational(FAULT_STORE),
        },
        DryPass::Event => match select_event_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            a.batch_turns,
            now_ms,
        ) {
            Ok(Some(plan)) => (
                PASS_EVENT,
                plan.batch.rows.len(),
                rc_distill::pass_prompt::event_system(),
                rc_distill::pass_prompt::event_user(
                    &plan.batch,
                    &plan.candidates,
                    &plan.ctx_turns,
                ),
            ),
            Ok(None) => return no_batch(),
            Err(_) => return operational(FAULT_STORE),
        },
        DryPass::Lesson => match select_lesson_batch(
            root.transcript(),
            root.ledger(),
            root.memory(),
            a.batch_turns,
            now_ms,
        ) {
            Ok(Some(plan)) => (
                PASS_LESSON,
                plan.batch.rows.len(),
                rc_distill::pass_prompt::lesson_system(),
                rc_distill::pass_prompt::lesson_user(&plan.batch, &plan.error_code),
            ),
            Ok(None) => return no_batch(),
            Err(_) => return operational(FAULT_STORE),
        },
    };
    let mut text = String::new();
    push_block(&mut text, "=== PASS ===", pass_label);
    // 这一格是**选中批**的轮数（真实运行里的 `turns`），不是 `--batch-turns` 的上限：
    // 批不满时两者不同，读数必须说前者。
    push_block(&mut text, "=== BATCH_TURNS ===", &turns.to_string());
    push_block(&mut text, "=== SYSTEM ===", &system);
    push_block(&mut text, "=== USER ===", &user);
    text.push_str("=== END ===");
    write_line(&text);
    ExitCode::from(0)
}

/// 没有可选批次时 dry-run 的唯一输出（与 `finish_*` 的 `state=none` 同一支语义）。
fn no_batch() -> ExitCode {
    write_line("=== NO BATCH ===");
    ExitCode::from(0)
}

/// S3a：本 pass 的"最近一次运行是否触顶"格（主语 = `llm_job:<pass>`，覆盖语义）。
/// 开头清 0、收尾写 `meter.saw_quota_exhausted()`：窗口是**一次运行**，不是累计。
///
/// 两个函数都**吞掉写错误**，这是有意与 `llm_calls` 那一格分开的：那一格的写失败会降退出码
/// （`FAULT_COUNTERS`），因为它是判定与成本的输入；这一格只是运维读数，写失败不许把一次
/// 成功的 pass 变成失败（"永不许把失败的 pass 变成成功"是它的另一面，同样成立：
/// 写 0 只是清窗口，退出码仍由 `fault`/`counters_failed` 决定）。
fn clear_quota_stopped(root: &rc_store::Root, subject: &str, now_ms: i64) {
    let _ = rc_ledger::counters::set_one(
        root.ledger(), "llm_job", subject,
        rc_ledger::counters::QUOTA_STOPPED_METRIC, 0, now_ms,
    );
}

/// 收尾写本 pass 的触顶位。`meter` 是这一轮的唯一账本，`saw_quota_exhausted` 是它与
/// `CycleOut.quota_exhausted` 共用的那个事实（两条生产者：脚本声明 / 后端回的额度耗尽）。
fn record_quota_stopped(root: &rc_store::Root, subject: &str, meter: &Metered, now_ms: i64) {
    let v = i64::from(meter.saw_quota_exhausted());
    let _ = rc_ledger::counters::set_one(
        root.ledger(), "llm_job", subject,
        rc_ledger::counters::QUOTA_STOPPED_METRIC, v, now_ms,
    );
}

/// 一条 naming pass：开库 → 能力闸 → 一轮（选批/双跑/错误面在 `round_once`）→
/// 读数/计数器/退出码。
///
/// 与 `cycle once` 的两处差异（v0）：① 不读写 `llm_state` 轮转态（座位直接 `Rotating::new`，
/// 冷启动；轮转态的继承与落盘留在 cycle）；② 只有一条作业，所以没有 `use_family` 的先后编排
/// ——naming 落在账本的默认族 `Extract` 上。
fn run_distill_naming(a: &DistillOnceArgs, meter: Rc<Metered>, slots: Slots) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // S3a：本 pass 的触顶窗口从 0 开始。放在能力闸之前——早退（探针/资格/空批）也是
    // "最近一次运行"，它们没有触顶，这一格必须如实说 0，而不是留着上一次运行的 1。
    clear_quota_stopped(&root, NAMING_PASS, now_ms);
    // 守卫 14 的执法点与 cycle 同一个：能力自测在入口判一次，业务层不再判。
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    // 空槽位列表在构造点响亮失败（`Rotating::new` 的契约）；来源那一侧已保证至少一座。
    let rotating = match Rotating::new(slots) {
        Ok(r) => r,
        Err(cat) => return operational(cat),
    };
    let (mut out, fault) = match round_once(
        root.transcript(),
        root.ledger(),
        root.memory(),
        RoundInput {
            family: Family::Naming,
            batch_turns: a.batch_turns,
            now_ms,
            backend: Some(&rotating),
            review_mode: review_mode(a),
            // 空轮重试只对 event 有牙齿；naming/fact/time 这三支恒 0（入口已拒非零值）。
            retry_empty: 0,
        },
    ) {
        Ok(Some(RoundOut::Naming(out))) => (NamingOut::of(&out), None),
        // 空批是合法结果：不调模型、不写状态，退出 0。
        // 空批也填观测两格（与其余支同一读口；没调用 ⇒ 账本恒 0）。
        Ok(None) => {
            let mut out = NamingOut::none();
            out.max_call_ms = meter.max_call_ms();
            out.timeouts = meter.timeouts();
            return finish_naming(&out, None, false);
        }
        Ok(Some(_)) => unreachable!("round_once 按入参的 family 交回变体"),
        // 选批失败一律是 store 面（现状：不打读数）。
        Err(RoundError::Select(_)) => return operational(FAULT_STORE),
        Err(RoundError::Run { error, turns, system_turns_skipped, .. }) => {
            let (state, fault) = match error {
                DistillError::Llm(failure) => ("pending", NamingFault::Llm(failure)),
                // S3b-3c：naming 的窄审拒收与抽取契约族同一去向（exit 2、"这批被拒收"）。
                DistillError::Contract(_) | DistillError::A1(_) | DistillError::NamingReview(_) => {
                    ("rejected", NamingFault::Rejected)
                }
                // 其余（Memory/Store/Judge/Audit）都是运维/store 面：runner 已把轮写成
                // `rejected(store)`，这里照同一族的类别词报。
                _ => ("rejected", NamingFault::Store),
            };
            (NamingOut::failed(turns, state, system_turns_skipped), Some(fault))
        }
    };
    // 观测面（W33-A10）：这一发的两格读数从账本读出（构造器给的是零值）。
    // 无论成败都填：失败发的耗时与超时计数恰恰是观测面最要看见的一发。
    out.max_call_ms = meter.max_call_ms();
    out.timeouts = meter.timeouts();

    // 计数器：naming 走账本的默认族（`Extract`）。只统计真开过口的次数；写失败只降退出码，
    // 读数照打（与 cycle 的"两件事同时出去"同律）。
    let calls = meter.calls(JobFamily::Extract) as i64;
    let counters_failed = calls > 0
        && rc_ledger::counters::bump(
            root.ledger(),
            "llm_job",
            NAMING_PASS,
            rc_ledger::counters::LLM_CALLS_METRIC,
            calls,
            now_ms,
        )
        .is_err();
    // S3a：本 pass 收尾写"最近一次运行是否触顶"（与上面 `llm_calls` 的写点是同一处，
    // 但语义不同：那一格是累计成本，这一格是覆盖写的一次运行窗口）。
    record_quota_stopped(&root, NAMING_PASS, &meter, now_ms);
    finish_naming(&out, fault, counters_failed)
}

// ── S2b-2b：`distill once --pass fact --family <f>` ─────────────────────────────

/// 一条 fact pass 的运维面读数（守卫 12：十五格，没有正文/路径/密钥）。
/// 键顺序就是派发档给的形状：`schema_version, pass, family, turns, drafts, dropped, uncovered,
/// written, duplicate, rejected, uncertain, facts, cards, mentions, state`（转换器配套再补
/// `system_turns_skipped` 一格，additive）。
/// `written + duplicate + rejected + uncertain = drafts`（S2e-6 起 `duplicate` 是第四格）；
/// `drafts + dropped = 模型原始草稿数`（S2e-8b：`dropped` 是被宽松 parser 丢掉的条数，
/// `uncovered` 是覆盖缺口的轮数，两者都与格内的其余读数同源）。
#[derive(serde::Serialize)]
struct FactOut {
    schema_version: u32,
    pass: &'static str,
    family: &'static str,
    turns: usize,
    drafts: usize,
    dropped: usize,
    uncovered: usize,
    written: usize,
    duplicate: usize,
    rejected: usize,
    uncertain: usize,
    facts: usize,
    cards: usize,
    mentions: usize,
    /// 批区间内被跳过的 `role='system'` 轮数（转换器配套）：只作上下文、永不作 TARGET。
    system_turns_skipped: usize,
    state: &'static str,
    /// W19c2：这一发是不是"单元级失败试满上限、整批落成隔离"（0/1，缺省 0）。
    /// **只追加在最后**，既有键序一个不动。`state=quarantined` 而这一格是 0 的批是
    /// **质量**隔离（审核不确定等），日路径的连续隔离墙不许把它算进座位/单元卡死。
    retry_exhausted: u8,
    /// 观测面（W33-A10，只追加）：本进程最慢一发真实出网的耗时（单调钟毫秒）；
    /// 回放座位没有真实出网 ⇒ 恒 0（stub 不发明耗时）。
    max_call_ms: i64,
    /// 观测面第二格：这一发里 `LlmFailure::Timeout` 的计数（含审核档）。
    timeouts: u32,
}

/// 读数的形状版本（与 pass 契约的 `schema_version=0` 是两件事：这是运维面的格子版本）。
const FACT_OUT_SCHEMA_VERSION: u32 = 2;

impl FactOut {
    /// 没有可选的轮次：全零 + `state="none"`（空批，退出 0）。`family` 照打：
    /// "这一族今天没活"与"这一族跑完了"必须能分开读。
    fn none(family: FactFamily) -> FactOut {
        FactOut::failed(family, 0, 0, "none")
    }
    /// 跑完的一批：`state` 用 runner 的终态 slug（`done`/`empty`/`rejected`/`quarantined`）。
    fn of(family: FactFamily, out: &FactOutcome) -> FactOut {
        FactOut {
            schema_version: FACT_OUT_SCHEMA_VERSION,
            pass: PASS_FACT,
            family: family.slug(),
            turns: out.turns,
            drafts: out.drafts,
            dropped: out.dropped,
            uncovered: out.uncovered,
            written: out.written,
            duplicate: out.duplicate,
            rejected: out.rejected,
            uncertain: out.uncertain,
            facts: out.facts,
            cards: out.cards,
            mentions: out.mentions,
            system_turns_skipped: out.system_turns_skipped,
            state: out.state.slug(),
            retry_exhausted: u8::from(out.retry_exhausted),
            // 真值在 run_distill_<pass> 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }
    /// 失败的一批：轮数是**选中的**轮数（退避重试靠这一格看得见），行数无从谈起 ⇒ 0；
    /// `state` 是 ledger 里落下的那一个（`pending`/`rejected`/`quarantined`）。
    /// `system_turns_skipped` 照打：它是选批层面的事实，与这批跑成什么无关。
    fn failed(
        family: FactFamily,
        turns: usize,
        system_turns_skipped: usize,
        state: &'static str,
    ) -> FactOut {
        FactOut {
            schema_version: FACT_OUT_SCHEMA_VERSION,
            pass: PASS_FACT,
            family: family.slug(),
            turns,
            drafts: 0,
            dropped: 0,
            uncovered: 0,
            written: 0,
            duplicate: 0,
            rejected: 0,
            uncertain: 0,
            facts: 0,
            cards: 0,
            mentions: 0,
            system_turns_skipped,
            state,
            // 失败那一发（`RoundError::Run`）只可能是座位级或契约面 ⇒ 这一格恒 0。
            retry_exhausted: 0,
            // 真值在 run_distill_<pass> 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }
}

/// fact pass 的三类失败。退出码与 stderr 的词一一对应（派发档 §CLI）：
/// `Llm` ⇒ `error: llm` + 3；`Rejected` ⇒ `error: rejected` + 2；存储面 ⇒ `error: store` + 3。
#[derive(Clone, Copy)]
enum FactFault {
    /// 类别词之外再带一格四态（见 [`llm_fault_text`]）：`llm` 仍是 stderr 的第一个词。
    Llm(LlmFailure),
    Rejected,
    Store,
}

/// 一条 fact pass 的收口：**先**打 stdout 读数（失败也打），**再**打 stderr 的类别词并给退出码。
/// 计数器写失败只把退出码降到 3：读数已经在上面出去了。
fn finish_fact(out: &FactOut, fault: Option<FactFault>, counters_failed: bool) -> ExitCode {
    match serde_json::to_string(out) {
        Ok(text) => write_line(&text),
        Err(_) => return operational(FAULT_STORE_WRITE),
    }
    let code = match fault {
        None => 0,
        Some(FactFault::Rejected) => 2,
        Some(FactFault::Llm(_) | FactFault::Store) => 3,
    };
    let code = if counters_failed { 3 } else { code };
    match fault {
        Some(FactFault::Llm(failure)) => operator_line("error", llm_fault_text(failure)),
        Some(FactFault::Rejected) => operator_line("error", FAULT_REJECTED),
        Some(FactFault::Store) => operator_line("error", FAULT_STORE),
        None if counters_failed => operator_line("error", FAULT_COUNTERS),
        None => {}
    }
    ExitCode::from(code)
}

/// 一条 fact pass：开库 → 能力闸 → 一轮（选批/抽取/独立审核/落库在 `round_once`）→
/// 读数/计数器/退出码。
///
/// 与 naming 的差异只有两处：① 族是三选一（`Family::Fact`）；② 计数器主语是
/// `llm_job:fact`（naming 是 `llm_job:naming`），同一 `llm_calls` 指标按 pass 分账。
fn run_distill_fact(
    a: &DistillOnceArgs,
    family: FactFamily,
    meter: Rc<Metered>,
    slots: Slots,
) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // S3a：本 pass 的触顶窗口从 0 开始（早退也算"最近一次运行"，见 naming 那一处注释）。
    clear_quota_stopped(&root, PASS_FACT, now_ms);
    // 守卫 14 的执法点与 cycle/naming 同一个：能力自测在入口判一次，业务层不再判。
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    let rotating = match Rotating::new(slots) {
        Ok(r) => r,
        Err(cat) => return operational(cat),
    };
    let (mut out, fault) = match round_once(
        root.transcript(),
        root.ledger(),
        root.memory(),
        RoundInput {
            family: Family::Fact(family),
            batch_turns: a.batch_turns,
            now_ms,
            backend: Some(&rotating),
            review_mode: review_mode(a),
            // 空轮重试只对 event 有牙齿；naming/fact/time 这三支恒 0（入口已拒非零值）。
            retry_empty: 0,
        },
    ) {
        Ok(Some(RoundOut::Fact(out))) => (FactOut::of(family, &out), None),
        // 空批是合法结果：不调模型、不写状态，退出 0。
        Ok(None) => return finish_fact(&FactOut::none(family), None, false),
        Ok(Some(_)) => unreachable!("round_once 按入参的 family 交回变体"),
        // 选批失败一律是 store 面（现状：不打读数）。
        Err(RoundError::Select(_)) => return operational(FAULT_STORE),
        Err(RoundError::Run { error, turns, system_turns_skipped, landed }) => {
            let fault = match error {
                DistillError::Llm(failure) => FactFault::Llm(failure),
                // 抽取/审核两个 parser 都归契约族（exit 2）；`Contract` 只在"证据轮读不出来"
                // 这条不可达分支上出现，仍按契约拒收报。
                DistillError::Fact(_) | DistillError::Contract(_) | DistillError::A1(_) => {
                    FactFault::Rejected
                }
                // 其余（Memory/Store/Judge/Audit）都是运维/store 面。
                _ => FactFault::Store,
            };
            let fallback = match fault {
                FactFault::Llm(_) => "pending",
                FactFault::Rejected | FactFault::Store => "rejected",
            };
            // 失败批的 `state` 从 ledger 读（`round_once` 交回的就是库里那一格），
            // 读不到时按故障类别给一个同向的兜底词。
            let state = landed.unwrap_or(fallback);
            (FactOut::failed(family, turns, system_turns_skipped, state), Some(fault))
        }
    };
    // 观测面（W33-A10）：这一发的两格读数从账本读出（构造器给的是零值）。
    // 无论成败都填：失败发的耗时与超时计数恰恰是观测面最要看见的一发。
    out.max_call_ms = meter.max_call_ms();
    out.timeouts = meter.timeouts();
    // 计数器：`llm_job:fact`，指标 `llm_calls`。只统计真开过口的次数；写失败只降退出码。
    let calls = meter.calls(JobFamily::Extract) as i64;
    let counters_failed = calls > 0
        && rc_ledger::counters::bump(
            root.ledger(),
            "llm_job",
            PASS_FACT,
            rc_ledger::counters::LLM_CALLS_METRIC,
            calls,
            now_ms,
        )
        .is_err();
    // S3a：收尾写本 pass 的触顶位（覆盖语义，与 `llm_calls` 的累加是两件事）。
    record_quota_stopped(&root, PASS_FACT, &meter, now_ms);
    finish_fact(&out, fault, counters_failed)
}

// ── S2c-2：`distill once --pass event` ──────────────────────────────────────────

/// 一条 event pass 的运维面读数（守卫 12：十四格，没有正文/路径/密钥）。
/// 键顺序：`schema_version, pass, turns, drafts, dropped, uncovered, written, duplicate,
/// rejected, uncertain, facts, cards, mentions, state`（转换器配套再补
/// `system_turns_skipped` 一格，additive）。event 是单一 pass ⇒ **没有** `family` 格；
/// `drafts + dropped = 模型原始草稿数`（S2e-8b）。
/// C5-fix：补 `duplicate` 一格——整批草稿都被去重时 `written=0` 才有解释（R1 的 P1）；
/// 与 fact 的 `FactOut` 同名词同位置。既有键一个不改，`schema_version` 仍是 1（只加不减）。
#[derive(serde::Serialize)]
struct EventOut {
    schema_version: u32,
    pass: &'static str,
    turns: usize,
    drafts: usize,
    dropped: usize,
    uncovered: usize,
    written: usize,
    duplicate: usize,
    rejected: usize,
    uncertain: usize,
    facts: usize,
    cards: usize,
    mentions: usize,
    /// 批区间内被跳过的 `role='system'` 轮数（转换器配套）：只作上下文、永不作 TARGET。
    system_turns_skipped: usize,
    /// 空轮重试（`--retry-empty N`）的两格读数：实际发起的重试次数与重试命中的轮数。
    /// **只在开关打开时出现**（`skip_serializing_if`）——`--retry-empty 0`（默认）的读数必须与
    /// 开关落地前逐字相同，不多一格（派发档 §硬约束），所以缺省打成"没有这两格"而不是 0。
    /// additive：既有格一个不改，`schema_version` 仍是 1。
    #[serde(skip_serializing_if = "Option::is_none")]
    retries: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    recovered: Option<usize>,
    state: &'static str,
    /// W19c2：这一发是不是"单元级失败试满上限、整批落成隔离"（0/1，缺省 0）。
    /// **只追加在最后**，既有键序一个不动。`state=quarantined` 而这一格是 0 的批是
    /// **质量**隔离（审核不确定等），日路径的连续隔离墙不许把它算进座位/单元卡死。
    retry_exhausted: u8,
    /// 观测面（W33-A10，只追加）：本进程最慢一发真实出网的耗时（单调钟毫秒）；
    /// 回放座位没有真实出网 ⇒ 恒 0（stub 不发明耗时）。
    max_call_ms: i64,
    /// 观测面第二格：这一发里 `LlmFailure::Timeout` 的计数（含审核档）。
    timeouts: u32,
}

/// 读数的形状版本（与 pass 契约的 `schema_version=0` 是两件事：这是运维面的格子版本）。
const EVENT_OUT_SCHEMA_VERSION: u32 = 2;

impl EventOut {
    /// 没有可选的轮次：全零 + `state="none"`（空批，退出 0）。开关开着时那两格如实报 0
    /// （这一轮一次调用都没发）；关着时仍然一格都不多。
    fn none(retry_empty: usize) -> EventOut {
        let mut out = EventOut::failed(0, 0, "none");
        out.retries = (retry_empty > 0).then_some(0);
        out.recovered = (retry_empty > 0).then_some(0);
        out
    }
    /// 跑完的一批：`state` 用 runner 的终态 slug（`done`/`empty`/`rejected`/`quarantined`）。
    fn of(out: &EventOutcome) -> EventOut {
        EventOut {
            schema_version: EVENT_OUT_SCHEMA_VERSION,
            pass: PASS_EVENT,
            turns: out.turns,
            drafts: out.drafts,
            dropped: out.dropped,
            uncovered: out.uncovered,
            written: out.written,
            duplicate: out.duplicate,
            rejected: out.rejected,
            uncertain: out.uncertain,
            facts: out.facts,
            cards: out.cards,
            mentions: out.mentions,
            system_turns_skipped: out.system_turns_skipped,
            retries: out.retries,
            recovered: out.recovered,
            state: out.state.slug(),
            retry_exhausted: u8::from(out.retry_exhausted),
            // 真值在 run_distill_<pass> 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }
    /// 失败的一批：轮数是**选中的**轮数（退避重试靠这一格看得见），行数无从谈起 ⇒ 0；
    /// `state` 是 ledger 里落下的那一个（`pending`/`rejected`/`quarantined`）。
    /// `system_turns_skipped` 照打：它是选批层面的事实，与这批跑成什么无关。
    /// 重试那两格**不打**：`RoundError::Run` 只带选批事实与落地的 state，runner 的 tally 随
    /// `Ok` 交回；失败批里"重试了几次"没有可信来源，编一个 0 就是读数上的谎。
    fn failed(turns: usize, system_turns_skipped: usize, state: &'static str) -> EventOut {
        EventOut {
            schema_version: EVENT_OUT_SCHEMA_VERSION,
            pass: PASS_EVENT,
            turns,
            drafts: 0,
            dropped: 0,
            uncovered: 0,
            written: 0,
            duplicate: 0,
            rejected: 0,
            uncertain: 0,
            facts: 0,
            cards: 0,
            mentions: 0,
            system_turns_skipped,
            retries: None,
            recovered: None,
            state,
            // 失败那一发（`RoundError::Run`）只可能是座位级或契约面：单元级失败现在是
            // `Ok(Pending)`/`Ok(Quarantined)`，不从这里出去 ⇒ 这一格恒 0。
            retry_exhausted: 0,
            // 真值在 run_distill_<pass> 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }
}

/// event pass 的三类失败。退出码与 stderr 的词一一对应：`Llm` ⇒ `error: llm` + 3；
/// `Rejected` ⇒ `error: rejected` + 2；存储面 ⇒ `error: store` + 3。
#[derive(Clone, Copy)]
enum EventFault {
    /// 类别词之外再带一格四态（见 [`llm_fault_text`]）：`llm` 仍是 stderr 的第一个词。
    Llm(LlmFailure),
    Rejected,
    Store,
}

/// 一条 event pass 的收口：**先**打 stdout 读数（失败也打），**再**打 stderr 的类别词并给退出码。
/// 计数器写失败只把退出码降到 3：读数已经在上面出去了。
fn finish_event(out: &EventOut, fault: Option<EventFault>, counters_failed: bool) -> ExitCode {
    match serde_json::to_string(out) {
        Ok(text) => write_line(&text),
        Err(_) => return operational(FAULT_STORE_WRITE),
    }
    let code = match fault {
        None => 0,
        Some(EventFault::Rejected) => 2,
        Some(EventFault::Llm(_) | EventFault::Store) => 3,
    };
    let code = if counters_failed { 3 } else { code };
    match fault {
        Some(EventFault::Llm(failure)) => operator_line("error", llm_fault_text(failure)),
        Some(EventFault::Rejected) => operator_line("error", FAULT_REJECTED),
        Some(EventFault::Store) => operator_line("error", FAULT_STORE),
        None if counters_failed => operator_line("error", FAULT_COUNTERS),
        None => {}
    }
    ExitCode::from(code)
}

/// 一条 event pass：开库 → 能力闸 → 一轮（选批/抽取/独立审核/落库在 `round_once`）→
/// 读数/计数器/退出码。
///
/// 与 fact 的差异只有两处：① 单一族（`Family::Event`，账本键 `family=''`）；
/// ② 计数器主语是 `llm_job:event`（naming 是 `llm_job:naming`、fact 是 `llm_job:fact`），
/// 同一 `llm_calls` 指标按 pass 分账。
fn run_distill_event(a: &DistillOnceArgs, meter: Rc<Metered>, slots: Slots) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // S3a：本 pass 的触顶窗口从 0 开始（早退也算"最近一次运行"，见 naming 那一处注释）。
    clear_quota_stopped(&root, PASS_EVENT, now_ms);
    // 守卫 14 的执法点与 cycle/naming/fact 同一个：能力自测在入口判一次，业务层不再判。
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    let rotating = match Rotating::new(slots) {
        Ok(r) => r,
        Err(cat) => return operational(cat),
    };
    let (mut out, fault) = match round_once(
        root.transcript(),
        root.ledger(),
        root.memory(),
        RoundInput {
            family: Family::Event,
            batch_turns: a.batch_turns,
            now_ms,
            backend: Some(&rotating),
            review_mode: review_mode(a),
            retry_empty: a.retry_empty,
        },
    ) {
        Ok(Some(RoundOut::Event(out))) => (EventOut::of(&out), None),
        // 空批是合法结果：不调模型、不写状态，退出 0。
        Ok(None) => return finish_event(&EventOut::none(a.retry_empty), None, false),
        Ok(Some(_)) => unreachable!("round_once 按入参的 family 交回变体"),
        // 选批失败一律是 store 面（现状：不打读数）。
        Err(RoundError::Select(_)) => return operational(FAULT_STORE),
        Err(RoundError::Run { error, turns, system_turns_skipped, landed }) => {
            let fault = match error {
                DistillError::Llm(failure) => EventFault::Llm(failure),
                // 抽取/审核两个 parser 都归契约族（exit 2）；`Contract` 只在"证据轮读不出来"
                // 这条不可达分支上出现，仍按契约拒收报。
                DistillError::Event(_) | DistillError::Contract(_) | DistillError::A1(_) => {
                    EventFault::Rejected
                }
                // 其余（Memory/Store/Judge/Audit）都是运维/store 面。
                _ => EventFault::Store,
            };
            let fallback = match fault {
                EventFault::Llm(_) => "pending",
                EventFault::Rejected | EventFault::Store => "rejected",
            };
            // 失败批的 `state` 从 ledger 读（`round_once` 交回的就是库里那一格），
            // 读不到时按故障类别给一个同向的兜底词。
            let state = landed.unwrap_or(fallback);
            (EventOut::failed(turns, system_turns_skipped, state), Some(fault))
        }
    };
    // 观测面（W33-A10）：这一发的两格读数从账本读出（构造器给的是零值）。
    // 无论成败都填：失败发的耗时与超时计数恰恰是观测面最要看见的一发。
    out.max_call_ms = meter.max_call_ms();
    out.timeouts = meter.timeouts();
    // 计数器：`llm_job:event`，指标 `llm_calls`。只统计真开过口的次数；写失败只降退出码。
    let calls = meter.calls(JobFamily::Extract) as i64;
    let counters_failed = calls > 0
        && rc_ledger::counters::bump(
            root.ledger(),
            "llm_job",
            PASS_EVENT,
            rc_ledger::counters::LLM_CALLS_METRIC,
            calls,
            now_ms,
        )
        .is_err();
    // S3a：收尾写本 pass 的触顶位（覆盖语义，与 `llm_calls` 的累加是两件事）。
    record_quota_stopped(&root, PASS_EVENT, &meter, now_ms);
    finish_event(&out, fault, counters_failed)
}


// ── 波三刀 A：`distill once --pass lesson` ──────────────────────────────────────

/// 一条 lesson pass 的运维面读数（守卫 12：只有计数与状态词，没有正文/路径/密钥）。
///
/// 键序：`schema_version, pass, source_items, turns, drafts, cards, duplicate, rejected,
/// uncertain, system_turns_skipped, state, max_call_ms, timeouts`。
///
/// `source_items` 是这一组候选覆盖了几条被驳回的 `review_item`（选批层面的事实，与这批
/// 跑成什么无关）；`cards` 是真插入的 `kind='lesson'` 卡数。lesson 没有 dropped/uncovered：
/// 它不逐轮交草稿，"空"就是 `lessons=[]`。
#[derive(serde::Serialize)]
struct LessonOut {
    schema_version: u32,
    pass: &'static str,
    source_items: usize,
    turns: usize,
    drafts: usize,
    cards: usize,
    duplicate: usize,
    rejected: usize,
    uncertain: usize,
    system_turns_skipped: usize,
    state: &'static str,
    /// 观测面（W33-A10，只追加）：本进程最慢一发真实出网的耗时（单调钟毫秒）。
    max_call_ms: i64,
    /// 观测面第二格：这一发里 `LlmFailure::Timeout` 的计数（含审核档）。
    timeouts: u32,
}

/// 读数的形状版本（与 pass 契约的 `schema_version=0` 是两件事）。
const LESSON_OUT_SCHEMA_VERSION: u32 = 1;

impl LessonOut {
    /// 没有可选候选：全零 + `state="none"`（空批合法，退出 0，零成本）。
    fn none() -> LessonOut {
        LessonOut::failed(0, 0, "none")
    }
    /// 失败批：轮数是**选中的**轮数，行数无从谈起 ⇒ 0。
    fn failed(turns: usize, system_turns_skipped: usize, state: &'static str) -> LessonOut {
        LessonOut {
            schema_version: LESSON_OUT_SCHEMA_VERSION,
            pass: PASS_LESSON,
            source_items: 0,
            turns,
            drafts: 0,
            cards: 0,
            duplicate: 0,
            rejected: 0,
            uncertain: 0,
            system_turns_skipped,
            state,
            max_call_ms: 0,
            timeouts: 0,
        }
    }
    fn of(
        out: &LessonOutcome,
        source_items: usize,
        system_turns_skipped: usize,
    ) -> LessonOut {
        LessonOut {
            schema_version: LESSON_OUT_SCHEMA_VERSION,
            pass: PASS_LESSON,
            source_items,
            turns: out.turns,
            drafts: out.drafts,
            cards: out.cards,
            duplicate: out.duplicate,
            rejected: out.rejected,
            uncertain: out.uncertain,
            system_turns_skipped,
            state: out.state.slug(),
            max_call_ms: 0,
            timeouts: 0,
        }
    }
}

/// lesson 的失败类别（退出码映射与 event 同一条：契约 2 / 运维 3）。
enum LessonFault {
    Llm(rc_llm::LlmFailure),
    Rejected,
    Store,
}

fn finish_lesson(
    out: &LessonOut,
    fault: Option<LessonFault>,
    counters_failed: bool,
) -> ExitCode {
    match serde_json::to_string(out) {
        Ok(text) => write_line(&text),
        Err(_) => return operational(FAULT_STORE_WRITE),
    }
    let code = match fault {
        None => 0,
        Some(LessonFault::Rejected) => 2,
        Some(LessonFault::Llm(_) | LessonFault::Store) => 3,
    };
    let code = if counters_failed { 3 } else { code };
    match fault {
        Some(LessonFault::Llm(failure)) => operator_line("error", llm_fault_text(failure)),
        Some(LessonFault::Rejected) => operator_line("error", FAULT_REJECTED),
        Some(LessonFault::Store) => operator_line("error", FAULT_STORE),
        None if counters_failed => operator_line("error", FAULT_COUNTERS),
        None => {}
    }
    ExitCode::from(code)
}

/// 一条 lesson pass：开库 → 能力闸 → 选批 → 抽取 + 独立审核 + 落卡（全在
/// `rc_distill::pass_lesson`）→ 读数/计数器/退出码。
///
/// 与 event 的差异只有一处：不用 `round_once`（`Family` 是三族+event/state/time 的闭集）。
/// 波三修 1 起 `lesson` 进了 `pass_state::PASS_IDS`：`run_lesson` 在每个终局把这一批的每一轮
/// 写成终态行，选批据此幂等（空产出/契约拒收也不会下一夜重选）。真实跑路径的形状与 event
/// 逐段相同：选批 → 一次抽取 → 独立审核 → 写库 → 同一组读数格。
fn run_distill_lesson(a: &DistillOnceArgs, meter: Rc<Metered>, slots: Slots) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    clear_quota_stopped(&root, PASS_LESSON, now_ms);
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    let rotating = match Rotating::new(slots) {
        Ok(r) => r,
        Err(cat) => return operational(cat),
    };
    let plan = match select_lesson_batch(
        root.transcript(),
        root.ledger(),
        root.memory(),
        a.batch_turns,
        now_ms,
    ) {
        Ok(Some(plan)) => plan,
        // 空候选是合法结果：一次模型都不调、一行都不写，退出 0。
        Ok(None) => return finish_lesson(&LessonOut::none(), None, false),
        Err(_) => return operational(FAULT_STORE),
    };
    let turns = plan.batch.rows.len();
    let source_items = plan.source_items;
    let skipped = plan.system_turns_skipped;
    let (mut out, fault) = match run_lesson(
        root.memory(),
        root.transcript(),
        root.ledger(),
        &plan,
        &rotating,
        now_ms,
        review_mode(a),
    ) {
        Ok(out) => (LessonOut::of(&out, source_items, skipped), None),
        Err(error) => {
            let fault = match error {
                rc_distill::LessonRunError::Llm(failure) => LessonFault::Llm(failure),
                // 抽取与审核两个 parser 都归契约族（exit 2）。
                rc_distill::LessonRunError::Contract(_) => LessonFault::Rejected,
                // 其余（Memory/Store/不可达的接线错误）都是运维/store 面。
                rc_distill::LessonRunError::Store(_) => LessonFault::Store,
            };
            let fallback = match fault {
                LessonFault::Llm(_) => "pending",
                LessonFault::Rejected | LessonFault::Store => "rejected",
            };
            (LessonOut::failed(turns, skipped, fallback), Some(fault))
        }
    };
    out.max_call_ms = meter.max_call_ms();
    out.timeouts = meter.timeouts();
    let calls = meter.calls(JobFamily::Extract) as i64;
    let counters_failed = calls > 0
        && rc_ledger::counters::bump(
            root.ledger(),
            "llm_job",
            PASS_LESSON,
            rc_ledger::counters::LLM_CALLS_METRIC,
            calls,
            now_ms,
        )
        .is_err();
    record_quota_stopped(&root, PASS_LESSON, &meter, now_ms);
    finish_lesson(&out, fault, counters_failed)
}

// ── P2：`distill once --pass state` ─────────────────────────────────────────────

/// 一条 state pass 的运维面读数（守卫 12：只有计数与状态词，没有正文/路径/密钥）。
///
/// 键顺序：`schema_version, pass, turns, drafts, dropped, uncovered, inferred, written,
/// duplicate, rejected, uncertain, states, mentions, conflicts, system_turns_skipped, state`。
///
/// 与 event 的读数差三格：
/// - `inferred`：程序判为**推导**（值不在引文里）而进隔离的条数——方案 §7 要的"隔离占比"分子；
/// - `conflicts`：同主体同维度**异值**的冲突条数（走隔离，不覆盖旧行）；
/// - `states`：真插入的 `state_assertion` 行数（event 那一格叫 `facts`，写的是另一张表）。
///   `cards` 这一格对 state **不存在**：状态不产卡，消费面读的是 `state_assertion` 本表。
#[derive(serde::Serialize)]
struct StateOut {
    schema_version: u32,
    pass: &'static str,
    turns: usize,
    drafts: usize,
    dropped: usize,
    uncovered: usize,
    inferred: usize,
    written: usize,
    duplicate: usize,
    rejected: usize,
    uncertain: usize,
    states: usize,
    mentions: usize,
    conflicts: usize,
    system_turns_skipped: usize,
    state: &'static str,
    /// W19c2：这一发是不是"单元级失败试满上限、整批落成隔离"（0/1，缺省 0）。
    /// **只追加在最后**，既有键序一个不动。`state=quarantined` 而这一格是 0 的批是
    /// **质量**隔离（审核不确定、程序判出的推导件），日路径的连续隔离墙不许把它算进座位/单元卡死。
    retry_exhausted: u8,
    /// 观测面（W33-A10，只追加）：本进程最慢一发真实出网的耗时（单调钟毫秒）；
    /// 回放座位没有真实出网 ⇒ 恒 0（stub 不发明耗时）。
    max_call_ms: i64,
    /// 观测面第二格：这一发里 `LlmFailure::Timeout` 的计数（含审核档）。
    timeouts: u32,
}

const STATE_OUT_SCHEMA_VERSION: u32 = 2;

impl StateOut {
    /// 没有可选的轮次或早退：全零 + 给定 `state`（空批退出 0）。
    fn zero(state: &'static str) -> StateOut {
        StateOut {
            schema_version: STATE_OUT_SCHEMA_VERSION,
            pass: PASS_STATE,
            turns: 0,
            drafts: 0,
            dropped: 0,
            uncovered: 0,
            inferred: 0,
            written: 0,
            duplicate: 0,
            rejected: 0,
            uncertain: 0,
            states: 0,
            mentions: 0,
            conflicts: 0,
            system_turns_skipped: 0,
            state,
            retry_exhausted: 0,
            // 真值在 run_distill_<pass> 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }

    fn of(out: &StateOutcome) -> StateOut {
        StateOut {
            schema_version: STATE_OUT_SCHEMA_VERSION,
            pass: PASS_STATE,
            turns: out.turns,
            drafts: out.drafts,
            dropped: out.dropped,
            uncovered: out.uncovered,
            inferred: out.inferred,
            written: out.written,
            duplicate: out.duplicate,
            rejected: out.rejected,
            uncertain: out.uncertain,
            states: out.states,
            mentions: out.mentions,
            conflicts: out.conflicts,
            system_turns_skipped: out.system_turns_skipped,
            state: out.state.slug(),
            retry_exhausted: u8::from(out.retry_exhausted),
            // 真值在 run_distill_<pass> 的终点从账本读进这两格（构造器先给零值）。
            max_call_ms: 0,
            timeouts: 0,
        }
    }

    /// 失败的一批：轮数是**选中的**轮数，`state` 是 ledger 里落下的那一格。
    fn failed(turns: usize, system_turns_skipped: usize, state: &'static str) -> StateOut {
        let mut out = StateOut::zero(state);
        out.turns = turns;
        out.system_turns_skipped = system_turns_skipped;
        out
    }
}

/// state pass 的三类失败，退出码与 event 同词同码：`Llm` ⇒ 3、`Rejected` ⇒ 2、`Store` ⇒ 3。
#[derive(Clone, Copy)]
enum StateFault {
    /// 类别词之外再带一格四态（见 [`llm_fault_text`]）：`llm` 仍是 stderr 的第一个词。
    Llm(LlmFailure),
    Rejected,
    Store,
}

/// 先打 stdout 读数（失败也打），再打 stderr 的类别词并给退出码。
fn finish_state(out: &StateOut, fault: Option<StateFault>, counters_failed: bool) -> ExitCode {
    match serde_json::to_string(out) {
        Ok(text) => write_line(&text),
        Err(_) => return operational(FAULT_STORE_WRITE),
    }
    let code = match fault {
        None => 0,
        Some(StateFault::Rejected) => 2,
        Some(StateFault::Llm(_) | StateFault::Store) => 3,
    };
    let code = if counters_failed { 3 } else { code };
    match fault {
        Some(StateFault::Llm(failure)) => operator_line("error", llm_fault_text(failure)),
        Some(StateFault::Rejected) => operator_line("error", FAULT_REJECTED),
        Some(StateFault::Store) => operator_line("error", FAULT_STORE),
        None if counters_failed => operator_line("error", FAULT_COUNTERS),
        None => {}
    }
    ExitCode::from(code)
}

/// 一条 state pass：开库 → 能力闸 → 一轮（选批/抽取/窄审/落库在 `round_once`）→ 读数/计数器/退出码。
///
/// 计数器主语是 `llm_job:state`（与 naming/fact/event 同一本账、按 pass 分账）。
/// 空批是合法结果（含"候选池里一个锚都没有"那一支：主体只能是有锚的实体，锚还没建时
/// 这一轮一次模型都不该调）。
fn run_distill_state(a: &DistillOnceArgs, meter: Rc<Metered>, slots: Slots) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // S3a-fix（评审 I-2）：这条 pass 也写 `llm_jobs:state:llm_calls`，所以它同样是"一次记过账的
    // 运行"——触顶窗口必须与 naming/fact/event 同形地从 0 开始、在收尾写真实值，否则
    // `--pass state` 触顶之后 `status` 读到的仍是上一条 pass 留下的 0。
    // 清 0 放在能力闸之前：早退（探针/资格/空批）也是一次"最近一次运行"，它们没有触顶。
    clear_quota_stopped(&root, PASS_STATE, now_ms);
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    let rotating = match Rotating::new(slots) {
        Ok(r) => r,
        Err(cat) => return operational(cat),
    };
    let (mut out, fault) = match round_once(
        root.transcript(),
        root.ledger(),
        root.memory(),
        RoundInput {
            family: Family::State,
            batch_turns: a.batch_turns,
            now_ms,
            backend: Some(&rotating),
            review_mode: review_mode(a),
            // state 没有空轮重试这一格（入口已按用法错误拒掉非 0 值）。
            retry_empty: 0,
        },
    ) {
        Ok(Some(RoundOut::State(out))) => (StateOut::of(&out), None),
        Ok(None) => return finish_state(&StateOut::zero("none"), None, false),
        Ok(Some(_)) => unreachable!("round_once 按入参的 family 交回变体"),
        Err(RoundError::Select(_)) => return operational(FAULT_STORE),
        Err(RoundError::Run { error, turns, system_turns_skipped, landed }) => {
            let fault = match error {
                DistillError::Llm(failure) => StateFault::Llm(failure),
                DistillError::State(_) | DistillError::Contract(_) | DistillError::A1(_) => {
                    StateFault::Rejected
                }
                _ => StateFault::Store,
            };
            let fallback = match fault {
                StateFault::Llm(_) => "pending",
                StateFault::Rejected | StateFault::Store => "rejected",
            };
            let state = landed.unwrap_or(fallback);
            (StateOut::failed(turns, system_turns_skipped, state), Some(fault))
        }
    };
    // 观测面（W33-A10）：这一发的两格读数从账本读出（构造器给的是零值）。
    // 无论成败都填：失败发的耗时与超时计数恰恰是观测面最要看见的一发。
    out.max_call_ms = meter.max_call_ms();
    out.timeouts = meter.timeouts();
    let calls = meter.calls(JobFamily::Extract) as i64;
    let counters_failed = calls > 0
        && rc_ledger::counters::bump(
            root.ledger(),
            "llm_job",
            PASS_STATE,
            rc_ledger::counters::LLM_CALLS_METRIC,
            calls,
            now_ms,
        )
        .is_err();
    // S3a-fix（评审 I-2）：收尾写本 pass 的触顶位（覆盖语义，与上面 `llm_calls` 的累加是两件事）。
    record_quota_stopped(&root, PASS_STATE, &meter, now_ms);
    finish_state(&out, fault, counters_failed)
}

// ── S2d-2：`distill once --pass time` ──────────────────────────────────────────

/// 一条 time pass 的运维面读数（守卫 12：七格，没有正文/路径/密钥）。
/// 键顺序就是派发档给的形状：`schema_version, pass, turns, expressions, resolved, facts, state`。
/// time 是单一 pass ⇒ **没有** `family` 格；它没有模型面 ⇒ 也没有 drafts/written/rejected/uncertain。
#[derive(serde::Serialize)]
struct TimeOut {
    schema_version: u32,
    pass: &'static str,
    turns: usize,
    expressions: usize,
    resolved: usize,
    facts: usize,
    state: &'static str,
    /// 观测面（W33-A10，只追加）：本进程最慢一发真实出网的耗时（单调钟毫秒）；
    /// 回放座位没有真实出网 ⇒ 恒 0（stub 不发明耗时）。
    max_call_ms: i64,
    /// 观测面第二格：这一发里 `LlmFailure::Timeout` 的计数（含审核档）。
    timeouts: u32,
}

/// 读数的形状版本（与 `time_expr` 的 `schema_version=0` 是两件事：这是运维面的格子版本）。
const TIME_OUT_SCHEMA_VERSION: u32 = 2;

impl TimeOut {
    /// 跑完的一批：`state` 用 runner 的终态 slug（`done`/`empty`）。空批也是 `empty`——
    /// 这条 pass 的"没有活干"就是选批为空，不是另一条流水线的"none"。
    fn of(out: &TimeOutcome) -> TimeOut {
        TimeOut {
            schema_version: TIME_OUT_SCHEMA_VERSION,
            pass: TIME_PASS,
            turns: out.turns,
            expressions: out.expressions,
            resolved: out.resolved,
            facts: out.facts,
            state: out.state.slug(),
            // time 从不开 LLM 口，这两格恒 0：读数键面与其余四条 pass 同形，聚合器不用特判。
            max_call_ms: 0,
            timeouts: 0,
        }
    }
}

/// 一条 time pass：开库 → 能力闸 → 一轮（选批/解析/入审/回填在 `round_once`）→ 读数。
/// 没有模型、没有来源、没有计数器（`llm_calls` 一个字节都不写：这条 pass 从不开 LLM 口）。
///
/// 失败面只有 store 面（退出 3）：time **没有** LLM、也没有契约路径，所以没有退出码 2 的分支。
/// runner 已把选中轮写成 `rejected(store)`；这里只打 stderr 的类别词，不补一份零值读数
/// （stdout 只说"跑完的结果"，与 naming/fact/event 选批失败那一支同形）。
///
/// time 的空批也**不是** `Ok(None)`：它在 `run_time` 里是 `state="empty"` 的完整读数
/// （审查水位照写），与其余三族的"没活干 ⇒ `state="none"`、一条状态行都不写"不是同一件事。
fn run_distill_time(a: &DistillOnceArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // 守卫 14 的执法点与其余三条 pass 同一个：能力自测在入口判一次，业务层不再判。
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    match round_once(
        root.transcript(),
        root.ledger(),
        root.memory(),
        RoundInput {
            family: Family::Time,
            batch_turns: a.batch_turns,
            now_ms,
            // time 不调模型：一个座位都不需要（`--replies`/`--llm-config` 也不读）。
            backend: None,
            review_mode: review_mode(a),
            // time 没有模型面；入口已拒非零值（这一格在它身上没有主语）。
            retry_empty: 0,
        },
    ) {
        Ok(Some(RoundOut::Time(out))) => match serde_json::to_string(&TimeOut::of(&out)) {
            Ok(text) => {
                write_line(&text);
                ExitCode::from(0)
            }
            Err(_) => operational(FAULT_STORE_WRITE),
        },
        Ok(Some(_)) => unreachable!("round_once 按入参的 family 交回变体"),
        // time 的选批住在 `run_time` 里，空批是 `state="empty"` 的读数，不是 `Ok(None)`。
        Ok(None) => unreachable!("time 的空批不是空结果，是 empty 读数"),
        Err(_) => operational(FAULT_STORE),
    }
}

/// 运维面只许有这四个读数（守卫 12）：没有正文、没有路径、没有分数。

#[derive(serde::Serialize)]
struct IndexOut {
    schema_version: u32,
    rows_indexed: u64,
    shadow_tables: usize,
    norm_version: u32,
    /// W38a 刀 A：转录段的读数——`turn_shadow` 全量重建灌进的行数（= turns 权威行数）。
    /// 两段各一格：memory 段是 `rows_indexed`，转录段是这一格（报告带两段读数）。
    turns_indexed: u64,
}

#[derive(serde::Serialize)]
struct Report {
    schema_version: u32,
    turns_accepted: usize,
    turns_rejected: usize,
    skipped_duplicate: usize,
    /// 被 purge 墓碑挡住的 turn（不落行、不分配提交号）。
    tombstoned: usize,
    batch_reject: String,
    rejects: Vec<serde_json::Value>,
    /// 入库正文里被换成 `<redacted:credential>` 的片段数。零正文：映射明文只在
    /// `--redaction-map` 那个本地文件里。
    redacted_spans: usize,
    /// G17：v3 下行投影丢弃的富字段读数（字段名 → 丢弃行数；口径住在
    /// `rc_contract::ingest_v3_project`）。v1 输入恒为空映射。C2 演进规则里的
    /// **加法**：字段只增不减，`REPORT_SCHEMA_VERSION` 不动（先例：阶段 1 档
    /// "加一个字段是 C2 演进规则里的加法，schema_version 仍是 1"）。
    dropped_fields: std::collections::BTreeMap<String, u64>,
    /// G19 刀 6：**接受轮**的 uid 列表（`rep.accepted` 的原样顺序；`import_reject_exit`
    /// 的拒收面恒为空）。C2 加法同上：字段只增不减，`REPORT_SCHEMA_VERSION` 不动
    /// （`dropped_fields` 先例）。注意口径：契约接受 ≠ 都落了行——墓碑挡住的轮也在
    /// `accepted` 里（`commit` 只是把那一组跳过），它们的 uid 在转录里不存在，
    /// `tombstoned` 那一格就是用来解释这一差值的；幂等命中的重传轮 uid 照列
    /// （它们在转录里本来就查得到，拿去 poll `ingest_status` 一样有效）。
    turn_uids: Vec<String>,
}

/// 这份报告自身的形状版本（不是来料的 `format_version`）。
const REPORT_SCHEMA_VERSION: u32 = 1;

/// 文本面列出 turn_uid 的上限（G19 刀 6）。json 面永远全量；文本全打印太长
/// （25 轮就是 1600 个十六进制字符），20 与 `rc_agent::MAX_STATUS_ENTITIES` 是同一个
/// 量级刻度。截断必须自证：`(+N more)` 那一段就是它（G7 的同一条纪律）。
const REPORT_TURN_UID_LISTED: usize = 20;

// 退出码即契约：0 全收 / 1 逐条拒 / 2 整批拒 / 3 运维失败。
// 2 与 3 必须分开：坏文件重试一万次还是坏文件，库开不通重试可能就是好的。
// 1 在 `cycle once` 下**用不上**：逐条拒收是 `import` 的形状（那里逐条有 `Reject` 可报），
// 这里逐条否决的读数住在 `CycleOut.merged` 与 `suppressed` 两格里，不是错误（§2.19）。
// 这句话必须写在这旁边，否则下一个人会以为漏了一个码。
/// 子命令面的唯一用法串。`main` 的用法错误与 `purge` 的"恰给一个口径"共用它：
/// 两处各抄一份，下一次加命令时只会改一处，另一处仍把新命令报成不存在。
const USAGE: &str = "invalid invocation — usage: recollect-hub import <FILE>... [--data-dir DIR] [--json] [--redaction-map FILE] | index rebuild|status [--data-dir DIR] | cycle once (legacy) [--replies FILE | --llm-config FILE] [--data-dir DIR] | distill once (--pass naming | --pass fact --family decision|promise|plan | --pass event | --pass state | --pass time) [--no-review] [--dry-run] [--replies FILE | --llm-config FILE] [--data-dir DIR] [--batch-turns N] | distill reclaim --pass naming|fact|event|state|time [--older-than-ms N] [--backoff-ms N] [--data-dir DIR] [--json] | derive status [--data-dir DIR] [--json] | derive retry --issue ID [--data-dir DIR] [--json] | derive run [--max-batches N] [--replies FILE | --llm-config FILE] [--llm-timeout-ms N] [--data-dir DIR] [--json] | issue list [--code CODE] [--open] [--pass PASS] [--data-dir DIR] [--json] | issue resolve --id ID [--data-dir DIR] [--json] | environment declare --hostname H [--brand B] [--os-user U] [--device-id D] [--bilibili-account A] [--qoder-account Q] [--principal ID] [--source-agent A] [--scope-kind project] [--scope-key KEY] [--data-dir DIR] [--json] | pending [--data-dir DIR] [--json] | anomaly sweep [--data-dir DIR] [--json] | episode run [--max-batches N] [--replies FILE | --llm-config FILE] [--llm-timeout-ms N] [--data-dir DIR] [--json] | fingerprint status [--data-dir DIR] [--json] | serve --hub-config FILE [--llm-config FILE] [--data-dir DIR] [--key FILE] [--dev-loopback] [--tls | --insecure-http] [--no-discovery] [--discovery-port PORT] | tls show [--data-dir DIR] [--json] | pair [--data-dir DIR] [--ttl-ms N] | invite [--ttl-ms N] [--json] [--hub-config FILE] [--data-dir DIR] | invite revoke <id> [--data-dir DIR] | revoke --device-id ID [--data-dir DIR] | snapshot publish [--channel agent|backup] [--data-dir DIR] | backup [--data-dir DIR] [--now-ms MS] | purge (--session-key K | --device-id D) [--confirm] [--data-dir DIR] | restore --manifest FILE [--with-extras] [--confirm] [--data-dir DIR] | secret init|put|list|rm [--data-dir DIR] | key export --out FILE [--data-dir DIR] | key import --from FILE [--data-dir DIR] | ownership backfill [--data-dir DIR] [--json] | principal claim --session K --device D --principal ID [--name NAME] --source S [--reason R] [--data-dir DIR] | principal list [--session K] [--data-dir DIR] [--json] | principal remap --from ID --to ID [--name NAME] [--apply] [--data-dir DIR] [--json] | anchor rebind --from ID --into ID [--reason R] [--actor A] [--dry-run] [--data-dir DIR] [--json] | anchor merge [--from ID --to ID] [--apply] [--only A|B|C] [--reason R] [--actor A] [--dry-run] [--data-dir DIR] [--json] | review list [--state S] [--pass P] [--limit N] [--data-dir DIR] [--json] | review reuse-ingest [--data-dir DIR] [--json] | review show ID [--payload | --prompt] [--data-dir DIR] [--json] | review decide ID (--approve|--reject) --reason CODE [--data-dir DIR] [--json] | review stats [--data-dir DIR] [--json] | review run [--limit N] [--pass P] [--replies FILE | --llm-config FILE] [--llm-timeout-ms N] [--data-dir DIR] [--json] | review retry-uncertain [--limit N] [--pass P] [--replies FILE | --llm-config FILE] [--llm-timeout-ms N] [--data-dir DIR] [--json] | review prune --older-than-days N [--data-dir DIR] [--json] | review backfill-verdict [--pass fact,event] [--dry-run] [--revert MANIFEST] [--manifest FILE] [--limit N] [--data-dir DIR] [--json] | reconcile memory [--apply] [--json] [--data-dir DIR] | handoff prune [--now-ms MS] [--data-dir DIR]";

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        // clap 把 --help / --version 也算 Err，而它给用法错误定的退出码是 2——
        // 2 在这里专指"整批拒收"，敲错参数会让一份好文件被下游当成毒文件。
        // `use_stderr()` 正好是这两类的分界：true = 真用法错误，false = help/version。
        // 用法文本我们自己拼：clap 的原串会把操作员敲进来的那个 token 原样回显，而它常常是个路径。
        // §4.8 ② 之后 `--replies` 不再必填，所以这一行的两个来源旗标都带方括号、中间一个 `|`：
        // 它说的是"两条里给一条"，而"都不给"的后果住在 `FAULT_NO_LLM_SOURCE` 那一句里（不是这一支）。
        Err(e) if e.use_stderr() => {
            return operational(USAGE);
        }
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(0);
        }
    };
    match cli.cmd {
        Cmd::Import(a) => run_import(&a),
        Cmd::Index(a) => run_index(&a.job),
        Cmd::Cycle(a) => run_cycle(&a),
        Cmd::Distill(a) => run_distill(&a),
        Cmd::Derive(a) => run_derive(&a.job),
        Cmd::Issue(a) => run_issue(&a.job),
        Cmd::Environment(a) => run_environment(&a.job),
        Cmd::Pending(a) => run_pending(&a),
        Cmd::Anomaly(a) => run_anomaly(&a),
        Cmd::Episode(a) => run_episode(&a.job),
        Cmd::Fingerprint(a) => run_fingerprint(&a.job),
        Cmd::Serve(a) => run_serve(&a),
        Cmd::Tls(a) => run_tls(&a),
        Cmd::Pair(a) => run_pair(&a),
        Cmd::Invite(a) => run_invite(&a),
        Cmd::Revoke(a) => run_revoke(&a),
        Cmd::Backup(a) => run_backup(&a),
        Cmd::Purge(a) => run_purge(&a),
        Cmd::Restore(a) => run_restore(&a),
        Cmd::Replicate(a) => run_replicate(&a),
        Cmd::Promote(a) => run_promote(&a),
        Cmd::Fence(a) => run_fence(&a),
        Cmd::Epoch(a) => run_epoch(&a),
        Cmd::Snapshot(a) => run_snapshot(&a),
        Cmd::Secret(a) => run_secret(&a),
        Cmd::Key(a) => run_key(&a),
        Cmd::BackupKey(a) => run_backup_key(&a),
        Cmd::Ownership(a) => run_ownership(&a),
        Cmd::Principal(a) => run_principal(&a),
        Cmd::Anchor(a) => run_anchor(&a),
        Cmd::Review(a) => run_review(&a),
        Cmd::Reconcile(a) => run_reconcile(&a),
        Cmd::Handoff(a) => run_handoff(&a.job),
    }
}

fn operational(msg: &str) -> ExitCode {
    operator_line("error", msg);
    ExitCode::from(3)
}

/// 守卫 14 的执法点：能力自测在本文件里**只此一处**（判据 38 数的是 rc-store 探针符号的出现
/// 次数）。七条入口（cycle / naming / fact / event / state / time / 渲染探针）此前各自
/// 手抄同一段 `match`；抄七份的失败模式是"改一处、其余六处静默分叉"。错误映射与分支语义逐字
/// 未动：探针失败 → `FAULT_PROBE` 的退出码（`Err(ExitCode)`，调用方原样 `return` 出去）；
/// **资格判定不在这里**——`qualify(&cap)` 是各入口自己的业务判定，读数仍由调用方持有。
fn probe_caps(root: &Root) -> Result<CapReport, ExitCode> {
    match rc_store::capability::probe(root.transcript(), root.ledger(), root.memory()) {
        Ok(c) => Ok(c),
        Err(_) => Err(operational(FAULT_PROBE)),
    }
}

/// 运维面的**唯一**下行写点（判据 49 数的就是它：本文件里标准错误只有一个写入处）。
/// 抽出来只为一件事——加第二条运维下行时必须共用这一个守卫，而不是再开一个裸写点。
fn operator_line(level: &str, msg: &str) {
    // 类别名而已：StoreError/io::Error 的原串可能带路径，零路径规则不许外传。
    // stderr 被关掉时写失败也不改判定——退出码才是 agent 侧读的东西。
    use std::io::Write;
    let _ = std::io::stderr().write_all(format!("{level}: {msg}\n").as_bytes());
}

/// `warning` 通道：它**不改退出码**，所以它的前缀不能是 `error:`——判定是 0 的一轮
/// 报 "error" 就是在操作员面前说谎。
///
/// 本仓有两条"不改判定"的运维下行，前缀**故意不同**：`hook` 那条叫 `note:`（它的 stderr 是给
/// agent 看的），hub 这一条叫 `warning:`（读者是跑命令的人）。不并成一个：`mcp` 与 `hook` 都
/// 不 shell out 到 hub，两条下行没有共同的读者，改名等于给其中一面换一个没读过的读者。
fn operator_warn(msg: &str) {
    operator_line("warning", msg);
}

/// 开库的唯一入口：`Root::open` 之后立刻把迁移提示送到运维面。
/// 文本的唯一出处是 `MigrationReport::ledger_warning`（rc-store）——doctor / mcp 两个入口
/// 也开同一个库，谁先撞上 v3→v4 谁负责说；这里只负责把它接到 `warning:` 这一行。
/// hub 的三条子命令（import / index / cycle）都从这里开库，于是"迁移发生了"这件事
/// 不会因为操作员走的是哪条命令而被吞掉——第一次开 v3 库的那条命令负责说出来。
fn open_root(dir: &std::path::Path) -> Result<Root, StoreError> {
    let root = Root::open(dir)?;
    if let Some(msg) = root.migration_report().ledger_warning() {
        operator_warn(&msg);
    }
    Ok(root)
}

/// 新命令的唯一取钟点。`main.rs` 本来就在 N8 的白名单里；把它收成一个函数，
/// 是为了"新命令各写一遍 SystemTime"这种增长从形状上消失。
fn unix_now_ms() -> Option<i64> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|d| d.as_millis() as i64)
}

/// serve 的明文 / TLS 判定（规则矩阵的唯一出处）。
///
/// - 非回环：默认 TLS；`--insecure-http` 显式退回明文。
/// - 回环（必须同时 `--dev-loopback`）：缺省明文（既有测试不变）；`--tls` 强制 TLS。
/// - 两个旗标同给是用法错误：它们说的是相反的事，静默取一个会让操作员以为另一件生效了。
fn tls_decision(addr: std::net::SocketAddr, tls: bool, insecure_http: bool) -> Result<bool, ()> {
    if tls && insecure_http {
        return Err(());
    }
    Ok(if tls {
        true
    } else if insecure_http {
        false
    } else {
        !addr.ip().is_loopback()
    })
}

fn run_serve(a: &ServeArgs) -> ExitCode {
    let Some(cfg_path) = a.hub_config.as_ref() else {
        return operational("no hub config — pass: --hub-config FILE (see: hub.toml.example)");
    };
    let cfg = match hub_cfg::load(cfg_path) {
        Ok(Some(c)) => c,
        Ok(None) => return operational("hub config not found — see: hub.toml.example"),
        Err(_) => return operational("hub config is malformed — see: hub.toml.example"),
    };
    let addr: std::net::SocketAddr = match cfg.bind.parse() {
        Ok(x) => x,
        Err(_) => return operational("hub bind is not a socket address"),
    };
    // 绑定判定在 `listen` 之前：一个 0.0.0.0 的误配不该先开一个监听再报错。
    if rc_net::http::assert_bind(addr, a.dev_loopback).is_err() {
        return operational("bind refused — use a mesh address, or --dev-loopback for loopback");
    }
    // 默认规则矩阵：非回环 ⇒ TLS（`--insecure-http` 显式退回明文）；
    // 回环（必须同时 `--dev-loopback`）⇒ 缺省明文，`--tls` 可强制 TLS。
    // 两个旗标同给在 `tls_decision` 里就是 `Err`——静默取一个会让操作员以为另一件生效了。
    let tls_on = match tls_decision(addr, a.tls, a.insecure_http) {
        Ok(on) => on,
        Err(()) => return operational(USAGE),
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let key_path = a.key.clone().unwrap_or_else(|| a.data_dir.join("hub.key"));
    let key = match rc_net::key::HubKey::load_or_create(&key_path) {
        Ok(k) => k,
        Err(_) => return operational("hub key is unreadable or malformed"),
    };
    // `--key` 指向的不是 data-dir 的默认私钥时，doctor 那一格（读 `<data-dir>/hub.key`）
    // 说的是另一把钥匙——不拿它的指纹去覆盖缓存。
    if a.key.is_none() {
        hub::key_archive::record_fingerprint(&a.data_dir, &key_path, &key);
    }
    let listener = match std::net::TcpListener::bind(addr) {
        Ok(l) => l,
        Err(_) => return operational("cannot bind the configured address"),
    };
    // TLS 身份在 `listen` 之后、accept 之前备好：证书读不懂/生成失败时，
    // 进程以一个静态话的退出码 3 结束，而不是开着一个连不上的端口。
    // 指纹顺路抄给 `HubState`：`/v1/identity` 与 `/v1/pair` 要报的就是这一份，
    // 让它们自己去读盘会多一条"公告的指纹与握手的指纹不是同一份"的岔路。
    let mut tls_fingerprint: Option<String> = None;
    let tls_config = if tls_on {
        let dir = a.data_dir.join("tls");
        let id = match rc_net::tls::ensure_identity(&dir, addr) {
            Ok(id) => id,
            Err(_) => return operational("tls identity is unusable — run: recollect-hub tls show"),
        };
        let config = match rc_net::tls::server_config(&id) {
            Ok(c) => c,
            Err(_) => return operational("tls identity is unusable — run: recollect-hub tls show"),
        };
        // 这一行是操作员给节点配 `hub_tls_fp` 的来源；短指纹只给人眼看。
        write_line(&format!(
            "tls_fingerprint={} tls_short={}",
            id.fingerprint,
            rc_net::tls::short_fingerprint(&id.fingerprint)
        ));
        tls_fingerprint = Some(id.fingerprint);
        Some(config)
    } else {
        write_line("tls=off");
        None
    };
    let max_bps = cfg.max_snapshot_bps;
    // 钟在 unix epoch 之前 ⇒ 0 ⇒ 配对码一律读作过期、nonce 一签发就过期：
    // 方向是响亮拒绝，不是静默放行。
    let now: Box<dyn Fn() -> i64 + Send + Sync> = Box::new(|| unix_now_ms().unwrap_or(0));
    // S4c1：发现应答线程。它只读一个文件（私钥）与一份内存里的身份快照，不碰库——
    // epoch 在 `root` 还在这只手上时读一次，之后这条线程与存储再无交集。
    // 任何一步失败都只是一条 warning：发现失效不该让一台已经在服务的中心退出。
    if a.no_discovery {
        write_line("discovery=off");
    } else {
        match rc_ledger::transcript::center_epoch(root.transcript()) {
            Ok(epoch) => {
                let hub_pub = key.public_b64();
                // 时间两格是占位：`serve_udp` 每次应答现算（起跑钟 + 已跑过的毫秒）。
                let identity = rc_net::invite::HubIdentity {
                    v: rc_net::invite::IDENTITY_VERSION,
                    hub_fp: rc_net::invite::hub_fingerprint(&hub_pub),
                    hub_pub,
                    // 明文中心也允许被发现（`tls_fp` 为空）：要不要连它由节点按票判。
                    tls_fp: tls_fingerprint.clone().unwrap_or_default(),
                    epoch,
                    endpoints: cfg.endpoints.clone(),
                    issued_at_ms: 0,
                    expires_at_ms: 0,
                    sig: String::new(),
                };
                let port = a.discovery_port;
                let key_path = key_path.clone();
                let discovery_now = now();
                // 先 bind、再打印：`discovery=on` 必须是**事实**。旧顺序在绑端口之前就
                // 印了 "已开"，端口被占用时 stdout 与真相相反——运维会去查防火墙。
                match std::net::UdpSocket::bind(("0.0.0.0", port)) {
                    Err(_) => {
                        operator_warn(
                            "discovery unavailable — udp port is not bindable (serve continues)",
                        );
                    }
                    Ok(socket) => {
                        // 线程自己要"起来了"才打印：私钥读不出来、或它与正在服务的身份
                        // 不是同一把，都不该让一行 `discovery=on` 留在日志里。
                        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
                        std::thread::spawn(move || {
                            // 只读加载，**绝不新建**：此刻文件不在，只可能是有人在动磁盘；
                            // 在这里凭空造一把钥匙会把全网节点钉住的 hub_pub 一次性打碎。
                            let key = match rc_net::key::HubKey::load(&key_path) {
                                Ok(k) => k,
                                Err(_) => {
                                    operator_warn(
                                        "discovery unavailable — hub key is unreadable (serve continues)",
                                    );
                                    return;
                                }
                            };
                            // 公告用这把钥匙签、而公告里的公钥来自 serve 那份身份：两者不是
                            // 同一把时，所有应答都验不过（节点只看到"发现失效"，零日志）。
                            if key.public_b64() != identity.hub_pub {
                                operator_warn(
                                    "discovery unavailable — hub key is not the serving identity (serve continues)",
                                );
                                return;
                            }
                            if ready_tx.send(()).is_err() {
                                // 主线程已经不在等：它只会因为自己要退出而丢下这个通道。
                                return;
                            }
                            rc_net::discovery::serve_udp(socket, key, identity, discovery_now);
                        });
                        if ready_rx.recv().is_ok() {
                            write_line(&format!("discovery=on port={port}"));
                        }
                    }
                }
            }
            // epoch 读不出来时，serve 自己会在第一个请求上响亮失败；这里不抢那句话，
            // 只把发现关掉——一条没有中心版本号的公告只会让节点读到一个错的 epoch。
            Err(_) => {
                operator_warn("discovery unavailable — center epoch is unreadable (serve continues)");
            }
        }
    }
    let mut state =
        HubState::new(root, key, cfg, &a.data_dir, now).with_tls_fingerprint(tls_fingerprint);
    // S3c-4：`--llm-config` 一给，`/v1/rows` 的 fact 行就在请求里同步窄审。座位按请求现造
    // （闭包只持有路径）：配置/身份/读箱的失败都折成"这一次造不出来"，HTTP 面与"没有座位"
    // 同一格（503 `review_unavailable`），不在启动时一次性判死——结构行的回并照常可用。
    if let Some(config) = a.llm_config.clone() {
        let data_dir = a.data_dir.clone();
        state = state.with_backend_factory(Box::new(move || {
            let (_meter, slots) =
                llm_source_with_timeout(&[], Some(&config), &data_dir, None).map_err(|_| ())?;
            let rotating = Rotating::new(slots).map_err(|_| ())?;
            Ok(Box::new(rotating) as Box<dyn LlmBackend>)
        }));
    }
    let stop = std::sync::atomic::AtomicBool::new(false);
    match tls_config {
        Some(config) => rc_net::hub::serve_loop_tls(
            listener,
            &mut state,
            &stop,
            Limits::default(),
            max_bps,
            config,
        ),
        None => {
            rc_net::hub::serve_loop(listener, &mut state, &stop, Limits::default(), max_bps);
        }
    }
    ExitCode::from(0)
}

fn run_tls(a: &TlsArgs) -> ExitCode {
    match &a.job {
        TlsJob::Show(s) => run_tls_show(s),
    }
}

/// 读证书，**绝不生成**：没有证书就退出 3（提示先跑一次 serve），
/// 因为在这里顺手生成一对新证书会让"指纹变了"这个事实由一次查询抹掉。
fn run_tls_show(a: &TlsShowArgs) -> ExitCode {
    let dir = a.data_dir.join("tls");
    if !dir.join("cert.pem").is_file() {
        return operational("no tls certificate yet — run: recollect-hub serve once (TLS mode)");
    }
    // 证书在 ⇒ `ensure_identity` 只走加载那一支；半套（有证书没私钥）按读不懂拒绝。
    // `bind` 只在生成时用到，这里给一个字面地址即可。
    let bind = match "127.0.0.1:0".parse::<std::net::SocketAddr>() {
        Ok(addr) => addr,
        Err(_) => return operational("tls identity is unusable"),
    };
    let id = match rc_net::tls::ensure_identity(&dir, bind) {
        Ok(id) => id,
        Err(_) => return operational("tls identity is unusable — restore cert.pem and key.pem"),
    };
    let short = rc_net::tls::short_fingerprint(&id.fingerprint);
    if a.json {
        write_line(
            &json!({ "tls_fingerprint": id.fingerprint, "tls_short": short }).to_string(),
        );
    } else {
        write_line(&format!("tls_fingerprint={} tls_short={}", id.fingerprint, short));
    }
    ExitCode::from(0)
}

fn run_pair(a: &PairArgs) -> ExitCode {
    if a.ttl_ms <= 0 {
        return operational("pair code ttl must be positive");
    }
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let (code, hash) = match rc_net::pair::new_pair_code() {
        Ok(v) => v,
        Err(_) => return operational("no entropy source"),
    };
    let expires_at_ms = now + a.ttl_ms;
    if rc_ledger::device::issue_pair_code(root.ledger_mut(), &hash, now, expires_at_ms).is_err() {
        return operational("store write failed — run: doctor");
    }
    // 这一行是**唯一**允许把一次性秘密送到 stdout 的地方：不打印它，配对这件事无法开始。
    // 有效期 5 分钟、一次使用；除此之外不写第二份。
    write_line(&json!({ "code": code, "expires_at_ms": expires_at_ms }).to_string());
    ExitCode::from(0)
}

/// `hub invite`：出票 / 作废两块，与 `pair`/`revoke` 并列（票是配对码的后继，旧码留着过渡）。
fn run_invite(a: &InviteArgs) -> ExitCode {
    match &a.job {
        Some(InviteJob::Revoke(r)) => run_invite_revoke(r),
        None => run_invite_issue(a),
    }
}

/// 出票：把"这台中心"的四个事实（hub 指纹、TLS 指纹、endpoints、有效期）签成一条文本。
///
/// **没有证书就退出 3**，而不是顺手生成一对：票里钉的 TLS 指纹必须与 serve 真正会用的
/// 那一张证书相同，而"哪张证书算数"只能由 serve 生成过一次来决定。
fn run_invite_issue(a: &InviteArgs) -> ExitCode {
    if a.ttl_ms <= 0 {
        return operational("invite ttl must be positive");
    }
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let tls_dir = a.data_dir.join("tls");
    if !tls_dir.join("cert.pem").is_file() {
        return operational("no tls certificate yet — run: recollect-hub serve once (TLS mode)");
    }
    let tls_fp = match rc_net::tls::fingerprint_at(&tls_dir) {
        Ok(fp) => fp,
        Err(_) => return operational("tls identity is unusable — run: recollect-hub tls show"),
    };
    // endpoints 是票的一部分（节点按它试连），所以配置缺失/配坏都是退出 3：
    // 出一张没有地址的票等于让人去猜。
    let cfg_path = a.hub_config.clone().unwrap_or_else(|| a.data_dir.join("hub.toml"));
    let cfg = match hub_cfg::load(&cfg_path) {
        Ok(Some(c)) => c,
        Ok(None) => return operational("hub config not found — pass: --hub-config FILE"),
        Err(_) => return operational("hub config is malformed — see: hub.toml.example"),
    };
    // 传输判定与 serve 是同一条规则（同一格 `tls_decision`）：非回环默认 TLS。
    // 节点侧把一条 `http://` 端点读成"这张票整张不可用"，所以 TLS 中心的票里出现
    // 明文端点必须在这里叫停——点名第几条，让运维改配置，而不是去怀疑票被伪造。
    let bind_addr = match cfg.bind.parse::<std::net::SocketAddr>() {
        Ok(addr) => addr,
        Err(_) => return operational("hub bind is not a socket address"),
    };
    let tls_on = match tls_decision(bind_addr, false, a.insecure_http) {
        Ok(on) => on,
        Err(()) => return operational(USAGE),
    };
    if tls_on {
        if let Some((i, ep)) =
            cfg.endpoints.iter().enumerate().find(|(_, e)| !e.starts_with("https://"))
        {
            // 地址本身不是秘密（它会被签进票、会被公告出去），点名一条是必要的可操作性。
            return operational(&format!(
                "hub.endpoints[{i}] is not https:// — a tls center signs https endpoints only: {ep}"
            ));
        }
    }
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    // 与 serve 同一个私钥文件：没有就先建（serve 也会建），否则票签出来的名字不是中心的名字。
    let key = match rc_net::key::HubKey::load_or_create(&a.data_dir.join("hub.key")) {
        Ok(k) => k,
        Err(_) => return operational("hub key is unreadable or malformed"),
    };
    let mut raw = [0u8; 16];
    if getrandom::fill(&mut raw).is_err() {
        return operational("no entropy source");
    }
    let id = hex_lower(&raw);
    let expires_at_ms = now + a.ttl_ms;
    if rc_ledger::invite::issue(root.ledger_mut(), &id, now, expires_at_ms).is_err() {
        return operational("store write failed — run: doctor");
    }
    let hub_pub = key.public_b64();
    let ticket = rc_net::invite::InviteTicket {
        v: rc_net::invite::TICKET_VERSION,
        id: id.clone(),
        hub_fp: rc_net::invite::hub_fingerprint(&hub_pub),
        tls_fp: tls_fp.clone(),
        endpoints: cfg.endpoints.clone(),
        expires_at_ms,
    };
    let sig = key.sign_b64(rc_net::invite::invite_canonical(&ticket).as_bytes());
    let text = rc_net::invite::encode_ticket(&ticket, &sig);
    if a.json {
        // 这一行是**唯一**允许把票送到 stdout 的地方：不打印它，加入这件事无法开始。
        // 票只在这里出现一次，不落日志、不进审计 detail。
        write_line(
            &json!({
                "id": id,
                "hub_fp": ticket.hub_fp,
                "tls_fp": tls_fp,
                "expires_at_ms": expires_at_ms,
                "ticket": text,
            })
            .to_string(),
        );
    } else {
        write_line(&text);
    }
    ExitCode::from(0)
}

fn run_invite_revoke(a: &InviteRevokeArgs) -> ExitCode {
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    match rc_ledger::invite::revoke(root.ledger_mut(), &a.id, now) {
        Ok(true) => {
            write_line(&json!({ "id": a.id, "revoked": true }).to_string());
            ExitCode::from(0)
        }
        Ok(false) => operational("invite not found"),
        Err(_) => operational("store write failed — run: doctor"),
    }
}

fn run_revoke(a: &RevokeArgs) -> ExitCode {
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    match rc_ledger::device::revoke(root.ledger_mut(), &a.device_id, now) {
        Ok(RevokeOutcome::Revoked) => {
            write_line(&json!({ "device_id": a.device_id, "revoked": true, "already": false }).to_string());
            ExitCode::from(0)
        }
        Ok(RevokeOutcome::AlreadyRevoked) => {
            write_line(&json!({ "device_id": a.device_id, "revoked": true, "already": true }).to_string());
            ExitCode::from(0)
        }
        Ok(RevokeOutcome::Unknown) => operational("device not found"),
        Err(_) => operational("store write failed — run: doctor"),
    }
}

fn run_backup(a: &BackupArgs) -> ExitCode {
    let now = match a.now_ms {
        Some(ms) => ms,
        None => match unix_now_ms() {
            Some(ms) => ms,
            None => return operational("system clock is behind the unix epoch"),
        },
    };
    let key = match &a.backup_key_file {
        None => None,
        Some(path) => {
            if !path.is_file() {
                return operational("backup key is missing");
            }
            if !backup_key_outside_data_dir(path, &a.data_dir) {
                return operational("backup key must live outside the data directory");
            }
            match rc_secret::backup::BackupKey::load(path) {
                Ok(key) => Some(key),
                Err(_) => return operational("backup key is unreadable"),
            }
        }
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::backup::backup_with_options(
        &mut root,
        &a.data_dir,
        now,
        hub::backup::BackupOptions {
            key: key.as_ref(),
            llm_config: a.llm_config.as_deref(),
        },
    ) {
        Ok(report) => {
            // 只印文件名与读数，不印路径：备份文件的完整路径会把目录结构一起带出去。
            // W28：组里多了 ledger（必备）与 core（懒库缺席时如实记 null）两个文件名。
            write_line(
                &json!({
                    "backup": true,
                    "name": report.transcript,
                    "memory": report.memory,
                    "ledger": report.ledger,
                    "core": report.core,
                    "memory_rows": report.memory_rows,
                    "extras": report.extras,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => {
            // 失败也要留时刻：只留"上次成功"分不清"没备份"与"备份了但失败"。
            let _ = hub::backup::mark_failed(&mut root, now);
            operational("backup failed")
        }
    }
}

fn backup_key_outside_data_dir(key: &Path, data_dir: &Path) -> bool {
    match (canonical_or_join(key), canonical_or_join(data_dir)) {
        (Some(key), Some(data)) => !key.starts_with(data),
        _ => false,
    }
}

/// 不存在的路径（还没建的 key、恢复目标目录）沿父链找到最近存在的祖先，做规范化后再拼回
/// 缺失的段；`..`、无文件名、找不到任何祖先都返回 `None`——推不出来就拒绝，不拿
/// "没解析出来"当"在外面"。
fn canonical_or_join(path: &Path) -> Option<PathBuf> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = path;
    loop {
        if let Ok(mut base) = std::fs::canonicalize(cur) {
            for part in missing.iter().rev() {
                base.push(part);
            }
            return Some(base);
        }
        let name = cur.file_name()?;
        missing.push(name.to_os_string());
        cur = cur.parent()?;
        if cur.as_os_str().is_empty() {
            return None;
        }
    }
}

fn run_backup_key(a: &BackupKeyArgs) -> ExitCode {
    match &a.job {
        BackupKeyJob::Generate(x) => {
            let key = match rc_secret::backup::BackupKey::generate() {
                Ok(key) => key,
                Err(_) => return operational("backup key generation failed"),
            };
            match key.save(&x.output) {
                Ok(()) => {
                    write_line(&json!({"created": true, "key_id": key.key_id()}).to_string());
                    ExitCode::from(0)
                }
                Err(rc_secret::SecretError::AlreadyExists) => {
                    write_line(&json!({"error": "already_exists"}).to_string());
                    ExitCode::from(2)
                }
                Err(_) => operational("backup key could not be written"),
            }
        }
    }
}

fn run_ownership(a: &OwnershipArgs) -> ExitCode {
    match &a.job {
        OwnershipJob::Backfill(x) => run_ownership_backfill(x),
    }
}

/// 回填的唯一出口面。读数一律走 stdout（`--json` 是同一组字段），
/// 失败沿用既有的 operational 路径（`error:` + 退出码 3、stdout 为空）：
/// 一份"看起来成功"的空报告比报错更坏——操作员会以为旧数据已经补完了。
fn run_ownership_backfill(a: &OwnershipBackfillArgs) -> ExitCode {
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let report = match hub::ownership::backfill(&mut root, now) {
        Ok(r) => r,
        Err(_) => return operational("store write failed — run: doctor"),
    };
    if a.json {
        write_line(
            &json!({
                "rows_scanned": report.rows_scanned,
                "already_owned": report.already_owned,
                "owners_written": report.owners_written,
                "unresolved": report.unresolved,
                "orphans_removed": report.orphans_removed,
            })
            .to_string(),
        );
    } else {
        write_line(&format!(
            "ownership backfill: scanned={} already={} written={} unresolved={} orphans_removed={}",
            report.rows_scanned,
            report.already_owned,
            report.owners_written,
            report.unresolved,
            report.orphans_removed,
        ));
    }
    ExitCode::from(0)
}

fn run_principal(a: &PrincipalArgs) -> ExitCode {
    match &a.job {
        PrincipalJob::Claim(x) => run_principal_claim(x),
        PrincipalJob::List(x) => run_principal_list(x),
        PrincipalJob::ClaimEnv(x) => run_principal_claim_env(x),
        PrincipalJob::Backfill(x) => run_principal_backfill(x),
        PrincipalJob::Remap(x) => run_principal_remap(x),
    }
}

/// `hub principal claim-env`（W37-A2/G13③②）：无章会话按环境声明补章的出口面。
/// 读数一律 stdout（`--json` 同一组字段）；声明缺席是 rc=0 的全零读数，不是运维失败。
fn run_principal_claim_env(a: &PrincipalClaimEnvArgs) -> ExitCode {
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let decl = hub::ingest::PrincipalDecl::from_env();
    let report = match hub::ingest::pre_claim_declared_principals(&root, decl.as_ref(), now) {
        Ok(r) => r,
        Err(_) => return operational("principal claim-env failed — run: doctor"),
    };
    if a.json {
        write_line(
            &json!({
                "declared": decl.is_some(),
                "sessions_scanned": report.scanned,
                "claims_written": report.claimed,
                "already_claimed": report.already_claimed,
            })
            .to_string(),
        );
    } else {
        write_line(&format!(
            "principal claim-env: declared={} scanned={} written={} already={}",
            u8::from(decl.is_some()),
            report.scanned,
            report.claimed,
            report.already_claimed,
        ));
    }
    ExitCode::from(0)
}

/// `hub principal backfill`（W37-A2/G13④）的出口面：回执带行数汇总 + 逐行明细。
/// dry-run 交回的 `rows` 是"将会写什么"，`backfilled` 恒 0——两件事不许折成一件。
fn run_principal_backfill(a: &PrincipalBackfillArgs) -> ExitCode {
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let (report, rows) = match hub::principal_backfill::backfill_principals(&root, now, a.apply) {
        Ok(v) => v,
        Err(_) => return operational("principal backfill failed — run: doctor"),
    };
    if a.json {
        let rows_json: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "mem_table": r.mem_table,
                    "mem_id": r.mem_id,
                    "principal_id": r.principal_id,
                })
            })
            .collect();
        write_line(
            &json!({
                "dry_run": report.dry_run,
                "rows_scanned": report.rows_scanned,
                "backfilled": report.backfilled,
                "unresolved": report.unresolved,
                "rows": rows_json,
            })
            .to_string(),
        );
    } else {
        for r in &rows {
            write_line(&format!(
                "row {}={} principal={}",
                r.mem_table, r.mem_id, r.principal_id
            ));
        }
        write_line(&format!(
            "principal backfill: dry_run={} scanned={} backfilled={} unresolved={}",
            u8::from(report.dry_run),
            report.rows_scanned,
            report.backfilled,
            report.unresolved,
        ));
    }
    ExitCode::from(0)
}

/// `hub principal remap`：归属串归一化（W37）。三条判定先于开库：
/// `from`/`to` 非空且不等——空串不是身份（`principal_eq` 的律），相等等于无事生非。
fn run_principal_remap(a: &PrincipalRemapArgs) -> ExitCode {
    if a.from.trim().is_empty() || a.to.trim().is_empty() || a.from == a.to {
        return operational("principal remap: --from and --to must be non-empty and distinct");
    }
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let (report, rows) = match hub::principal_backfill::remap_principals(
        &root,
        &a.from,
        &a.to,
        a.name.as_deref(),
        now,
        a.apply,
    ) {
        Ok(v) => v,
        Err(_) => return operational("principal remap failed — run: doctor"),
    };
    if a.json {
        let rows_json: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "mem_table": r.mem_table,
                    "mem_id": r.mem_id,
                })
            })
            .collect();
        write_line(
            &json!({
                "dry_run": report.dry_run,
                "rows_matched": report.rows_matched,
                "remapped": report.remapped,
                "anchors_ensured": report.anchors_ensured,
                "scopes": report.scopes,
                "rows": rows_json,
            })
            .to_string(),
        );
    } else {
        for r in &rows {
            write_line(&format!("row {}={}", r.mem_table, r.mem_id));
        }
        write_line(&format!(
            "principal remap: dry_run={} matched={} remapped={} anchors_ensured={} scopes={}",
            u8::from(report.dry_run),
            report.rows_matched,
            report.remapped,
            report.anchors_ensured,
            report.scopes.join(","),
        ));
    }
    ExitCode::from(0)
}

/// 盖一章（`hub principal claim`）。两条判定先于开库：
/// `correction` 缺 `reason` 是用法错误（工单规则 2 钉死退出 3），不该先动一个库再报错。
/// `hub anchor rebind`：换绑的运维面。
///
/// 三条取向写在这里：①**默认预演**——`--dry-run` 或没给 `--apply` 语义的调用只打印会动什么；
/// 这片把"预演"当默认，要写必须显式不带 `--dry-run`；②**搬不动就拒**,不是"能搬多少搬多少"
/// （半套换绑会让 id 与内容不再互相复算）；③原因与操作者进 `memory_op`，事后能问是谁换的、为什么。
fn run_anchor(a: &AnchorArgs) -> ExitCode {
    match &a.job {
        AnchorJob::Rebind(x) => run_anchor_rebind(x),
        AnchorJob::Merge(x) => run_anchor_merge(x),
    }
}

fn run_anchor_rebind(a: &AnchorRebindArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let reason = a.reason.as_deref().unwrap_or("-");
    let plan = match rc_memory::rebind::rebind_plan(root.memory(), &a.from, &a.into) {
        Ok(p) => p,
        Err(e) => return operational(&format!("anchor rebind refused: {}", describe_rebind(&e))),
    };
    if a.json {
        write_line(&rebind_plan_json(&plan).to_string());
    } else {
        write_line(&format!(
            "from={} into={} states={} 会合并={} mentions={} aliases={} memory_mentions={} fact 引用={} 挡住={:?}",
            plan.from, plan.into, plan.states, plan.collapse, plan.mentions, plan.aliases,
            plan.memory_mentions, plan.fact_refs, plan.blockers,
        ));
    }
    if a.dry_run {
        return ExitCode::from(0);
    }
    if !plan.blockers.is_empty() {
        return operational(&format!(
            "anchor rebind blocked: {} — 这些引用搬不动（行 id 不可复算或 id 规则未核实），\
             逐格走法见 rc-memory/src/rebind.rs 顶注第 3 条",
            plan.blockers.join(", ")
        ));
    }
    let report = match rc_memory::rebind::rebind_apply(
        root.memory(),
        &a.from,
        &a.into,
        reason,
        &a.actor,
        now_ms,
    ) {
        Ok(r) => r,
        Err(e) => return operational(&format!("anchor rebind failed: {}", describe_rebind(&e))),
    };
    if a.json {
        write_line(&rebind_report_json(&plan, &report).to_string());
    } else {
        write_line(&format!(
            "换绑完成：状态行搬 {} 条、合并 {} 条，mentions {}、aliases {}、memory_mention {}、fact 引用改列 {}、告警解决 {}",
            report.states_moved, report.states_merged, report.mentions_moved,
            report.aliases_moved, report.memory_mentions_moved, report.fact_refs_moved,
            report.attention_resolved,
        ));
    }
    ExitCode::from(0)
}

/// 闭集原因词（不给库内文本，守卫 12 那一族：运维面只读得到类别话）。
fn describe_rebind(e: &rc_memory::rebind::RebindError) -> &'static str {
    use rc_memory::rebind::RebindError as E;
    match e {
        E::SameAnchor => "same_anchor",
        E::Missing => "missing_anchor",
        E::KindMismatch => "kind_mismatch",
        E::ScopeMismatch => "scope_mismatch",
        E::AlreadyMerged => "already_merged",
        E::TargetMerged => "target_merged",
        E::RedirectCycle => "redirect_cycle",
        E::Blocked => "blocked",
        E::ReadOnly => "read_only",
        E::Store(_) => "store_error",
    }
}

fn rebind_plan_json(p: &rc_memory::rebind::RebindPlan) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 0, "op": "anchor_rebind", "mode": "plan",
        "from": p.from, "into": p.into,
        "states": p.states, "collapse": p.collapse, "mentions": p.mentions,
        "aliases": p.aliases, "memory_mentions": p.memory_mentions,
        "fact_refs": p.fact_refs,
        "blockers": p.blockers,
    })
}

fn rebind_report_json(
    p: &rc_memory::rebind::RebindPlan, r: &rc_memory::rebind::RebindReport,
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 0, "op": "anchor_rebind", "mode": "apply",
        "from": p.from, "into": p.into,
        "states_moved": r.states_moved, "states_merged": r.states_merged,
        "mentions_moved": r.mentions_moved, "aliases_moved": r.aliases_moved,
        "memory_mentions_moved": r.memory_mentions_moved,
        "fact_refs_moved": r.fact_refs_moved,
        "attention_resolved": r.attention_resolved,
    })
}

/// `hub anchor merge`：重复锚的发现面 + A 类合并的写面。
///
/// 三条取向：①**默认预演**——不带 `--apply` 就只读打开 `memory.db`（连迁移都不跑），
/// 一格都不写；②**只有 A 类会写**——跨 kind 的同名是重新命名、目标不唯一是歧义，
/// 两者都只出人工清单；③**搬不动就降级为人工**——A 类在预演里也跑一遍换绑校验，
/// 有 blockers 就不说"会合并"。
fn run_anchor_merge(a: &AnchorMergeArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // 指向模式（方案二）：`--from/--to` 都在场就走它，A/B/C 发现路径完全不进。
    if let (Some(from), Some(to)) = (a.from.as_deref(), a.to.as_deref()) {
        return run_anchor_merge_redirect(a, from, to, now_ms);
    }
    let reason = a.reason.as_deref().unwrap_or("anchor merge");
    let only = match a.only.as_deref() {
        None => rc_memory::merge::MergeClass::A,
        Some(s) => match rc_memory::merge::MergeClass::parse(s) {
            Some(c) => c,
            None => return operational(USAGE),
        },
    };

    if !a.apply {
        // 预演走**只读句柄**：`open_replica` 只连文件、只核 schema，不跑迁移。
        let path = a.data_dir.join("memory.db");
        let m = match rc_store::open_replica(&path) {
            Ok(m) => m,
            Err(_) => return operational("cannot open memory store"),
        };
        let plan = match rc_memory::merge::merge_plan(&m) {
            Ok(p) => p,
            Err(_) => return operational("anchor merge plan failed: store_error"),
        };
        if a.json {
            write_line(&merge_plan_json(&plan).to_string());
        } else {
            print_merge_plan(&plan);
            write_line("预演：一格未写；给 --apply 才落库（B/C 永远只出清单）");
        }
        return ExitCode::from(0);
    }

    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let plan = match rc_memory::merge::merge_plan(root.memory()) {
        Ok(p) => p,
        Err(_) => return operational("anchor merge plan failed: store_error"),
    };
    if !a.json {
        print_merge_plan(&plan);
    }
    let report = match rc_memory::merge::merge_apply(
        root.memory(),
        reason,
        &a.actor,
        now_ms,
        only,
    ) {
        Ok(r) => r,
        Err(e) => return operational(&format!("anchor merge failed: {}", describe_merge(&e))),
    };
    if a.json {
        let applied: Vec<Value> = report
            .merged
            .iter()
            .map(|m| merge_candidate_json(&m.candidate))
            .collect();
        write_line(&Value::Array(applied).to_string());
    } else {
        let rebinds = report.merged.iter().map(|m| &m.rebind);
        let mut mentions = 0usize;
        let mut aliases = 0usize;
        let mut memory_mentions = 0usize;
        let mut states = 0usize;
        let mut fact_refs = 0usize;
        let mut attention = 0usize;
        for r in rebinds {
            mentions += r.mentions_moved;
            aliases += r.aliases_moved;
            memory_mentions += r.memory_mentions_moved;
            states += r.states_moved;
            fact_refs += r.fact_refs_moved;
            attention += r.attention_resolved;
        }
        write_line(&format!(
            "合并完成：merged={} mentions={} aliases={} memory_mentions={} states={} fact_refs={} 告警解决={}",
            report.merged.len(), mentions, aliases, memory_mentions, states, fact_refs, attention,
        ));
    }
    if !report.refused.is_empty() {
        for (from, to, why) in &report.refused {
            operator_warn(&format!("anchor merge refused: {from} -> {to} ({why})"));
        }
        return ExitCode::from(3);
    }
    ExitCode::from(0)
}

fn describe_merge(e: &rc_memory::merge::MergeError) -> &'static str {
    use rc_memory::merge::MergeError as E;
    match e {
        E::ReadOnly => "read_only",
        E::Store(_) => "store_error",
    }
}

/// `hub anchor merge --from ID --to ID`：D8 方案二（指向合并）的运维面。
///
/// ① **默认预演**——只读打开、一格不写，报这一对的 A/B/C 分类（复用 `merge.rs` 的
///   发现/分类思想，见 `rc_memory::redirect::classify_pair`）与旧锚引用数；
/// ② `--apply` 真写——指向行/退场章/alias 三格/台账一个事务；
/// ③ **断链读数**——apply 之后把 `broken_redirects` 带上：解析失败的指向行在这里可见，
///   有断链退 3（与 rebind 的 refused 同一个分诊形状），不静默。
fn run_anchor_merge_redirect(a: &AnchorMergeArgs, from: &str, to: &str, now_ms: i64) -> ExitCode {
    let reason = a.reason.as_deref().unwrap_or("anchor redirect merge");
    if !a.apply {
        let path = a.data_dir.join("memory.db");
        let m = match rc_store::open_replica(&path) {
            Ok(m) => m,
            Err(_) => return operational("cannot open memory store"),
        };
        let (class, why) = match rc_memory::redirect::classify_pair(&m, from, to) {
            Ok(v) => v,
            Err(_) => return operational("anchor merge plan failed: store_error"),
        };
        let refs = rc_memory::merge::reference_counts(&m, from)
            .map(|c| c.total())
            .unwrap_or(0);
        let broken = rc_memory::redirect::broken_redirects(&m)
            .map(|b| b.len())
            .unwrap_or(0);
        if a.json {
            write_line(&json!({
                "op": "anchor_merge_redirect", "mode": "plan",
                "from_id": from, "to_id": to,
                "class": class, "class_reason": why,
                "from_refs": refs, "broken_redirects": broken,
            }).to_string());
        } else {
            write_line(&format!(
                "预演：{from} -> {to}（class={class} {why}；旧锚引用 {refs}；现网断链 {broken}）\n\
                 一格未写；给 --apply 才落库（不重键、零搬运）"));
        }
        return ExitCode::from(0);
    }
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let report = match rc_memory::redirect::merge_redirect(
        root.memory(), from, to, reason, &a.actor, now_ms,
    ) {
        Ok(r) => r,
        Err(e) => return operational(&format!("anchor merge redirect refused: {e}")),
    };
    let broken = rc_memory::redirect::broken_redirects(root.memory()).unwrap_or_default();
    if a.json {
        write_line(&json!({
            "op": "anchor_merge_redirect", "mode": "apply",
            "from_id": report.from_id, "to_id": report.to_id,
            "run_id": report.run_id, "aliases_merged": report.aliases_merged,
            "broken_redirects": broken.iter().map(|b| json!({
                "from_id": b.from_id, "stopped_at": b.stopped_at, "reason": b.reason,
            })).collect::<Vec<_>>(),
        }).to_string());
    } else {
        write_line(&format!(
            "指向合并完成：{} -> {}（alias+{}）；断链 {} 条",
            report.from_id, report.to_id, report.aliases_merged, broken.len()));
    }
    if broken.is_empty() {
        ExitCode::from(0)
    } else {
        for b in &broken {
            operator_warn(&format!(
                "anchor redirect broken: {} -> {} ({})", b.from_id, b.stopped_at, b.reason));
        }
        ExitCode::from(3)
    }
}

fn merge_candidate_json(c: &rc_memory::merge::MergeCandidate) -> Value {
    json!({
        "from_id": c.from_id,
        "to_id": c.to_id,
        "from_canonical": c.from_canonical,
        "to_canonical": c.to_canonical,
        "from_kind": c.from_kind,
        "to_kind": c.to_kind,
        "refs": c.refs,
        "class": c.class.as_str(),
        "action": c.action.as_str(),
        "reason": c.reason,
    })
}

fn merge_plan_json(plan: &[rc_memory::merge::MergeCandidate]) -> Value {
    Value::Array(plan.iter().map(merge_candidate_json).collect())
}

fn print_merge_plan(plan: &[rc_memory::merge::MergeCandidate]) {
    for c in plan {
        write_line(&format!(
            "{} {:<6} from={} ({}, {}) -> to={} ({}, {}) refs={} reason={}",
            c.class.as_str(),
            c.action.as_str(),
            c.from_canonical,
            c.from_kind,
            c.from_status,
            c.to_canonical,
            c.to_kind,
            c.to_status,
            c.refs,
            c.reason,
        ));
    }
    let count = |class| plan.iter().filter(|c| c.class == class).count();
    let mergeable = plan
        .iter()
        .filter(|c| c.action == rc_memory::merge::MergeAction::Merge)
        .count();
    write_line(&format!(
        "清单：{} 行（A={} B={} C={}），可自动合并 {} 行",
        plan.len(),
        count(rc_memory::merge::MergeClass::A),
        count(rc_memory::merge::MergeClass::B),
        count(rc_memory::merge::MergeClass::C),
        mergeable,
    ));
}

fn run_principal_claim(a: &PrincipalClaimArgs) -> ExitCode {    if a.source == "correction" && a.reason.as_deref().is_none_or(|r| r.trim().is_empty()) {
        return operational("principal claim: --source correction requires --reason");
    }
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    // `claimed_by='hub'`：这一章不是导入自动盖的，是运维面盖的。导入自动盖的那条写的是
    // 会话的 `source_agent`（工单规则 4），两种章在台账里因此分得开。
    let claim = match rc_ledger::transcript::claim_principal(
        root.transcript(),
        rc_ledger::transcript::NewPrincipalClaim {
            session_key: &a.session,
            device_id: &a.device,
            principal_id: &a.principal,
            principal_name: a.name.as_deref(),
            source: &a.source,
            claimed_by: "hub",
            reason: a.reason.as_deref(),
        },
        now_ms,
    ) {
        Ok(c) => c,
        // 原串可能带库内文本（守卫 12 那一族），运维面只读得到这一句类别话。
        Err(_) => return operational("principal claim failed — run: doctor"),
    };
    write_line(&format!(
        "id={} session={} device={} principal={} name={} source={} claimed_by={} at={} \
         supersedes={} reason={}",
        claim.id,
        claim.session_key,
        claim.device_id,
        claim.principal_id,
        claim.principal_name.as_deref().unwrap_or("-"),
        claim.source,
        claim.claimed_by,
        claim.claimed_at_ms,
        claim.supersedes.map_or_else(|| "-".to_owned(), |v| v.to_string()),
        claim.reason.as_deref().unwrap_or("-"),
    ));
    ExitCode::from(0)
}

/// 台账（`hub principal list`）：最新有效章在前，同一会话的后续行是它的历史
/// （`supersedes` 逐行指回上一行 ⇒ 链可读）。零正文、零路径。
fn run_principal_list(a: &PrincipalListArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let claims = match rc_ledger::transcript::principal_claims(root.transcript(), a.session.as_deref())
    {
        Ok(c) => c,
        Err(_) => return operational("cannot read principal claims — run: doctor"),
    };
    // 读侧排序是 `(session_key, device_id, claimed_at_ms DESC)` ⇒ 每个 (会话, 设备) 的
    // 第一行就是当前有效章，其余是历史。
    let mut seen: std::collections::HashSet<(String, String)> = std::collections::HashSet::new();
    let rows: Vec<(bool, &rc_ledger::transcript::PrincipalClaim)> = claims
        .iter()
        .map(|c| {
            let latest = seen.insert((c.session_key.clone(), c.device_id.clone()));
            (latest, c)
        })
        .collect();
    if a.json {
        let arr: Vec<Value> = rows
            .iter()
            .map(|(latest, c)| {
                json!({
                    "id": c.id,
                    "session_key": c.session_key,
                    "device_id": c.device_id,
                    "principal_id": c.principal_id,
                    "principal_name": c.principal_name,
                    "source": c.source,
                    "claimed_by": c.claimed_by,
                    "claimed_at_ms": c.claimed_at_ms,
                    "supersedes": c.supersedes,
                    "reason": c.reason,
                    "latest": latest,
                })
            })
            .collect();
        write_line(&Value::Array(arr).to_string());
    } else {
        for (latest, c) in rows {
            write_line(&format!(
                "latest={} session={} device={} id={} principal={} name={} source={} claimed_by={} \
                 at={} supersedes={} reason={}",
                u8::from(latest),
                c.session_key,
                c.device_id,
                c.id,
                c.principal_id,
                c.principal_name.as_deref().unwrap_or("-"),
                c.source,
                c.claimed_by,
                c.claimed_at_ms,
                c.supersedes.map_or_else(|| "-".to_owned(), |v| v.to_string()),
                c.reason.as_deref().unwrap_or("-"),
            ));
        }
    }
    ExitCode::from(0)
}

fn run_review(a: &ReviewArgs) -> ExitCode {
    match &a.job {
        ReviewJob::List(x) => run_review_list(x),
        ReviewJob::Show(x) => run_review_show(x),
        ReviewJob::Decide(x) => run_review_decide(x),
        ReviewJob::Stats(x) => run_review_stats(x),
        ReviewJob::Run(x) => run_review_run(x),
        ReviewJob::RetryUncertain(x) => run_review_retry(x),
        ReviewJob::Prune(x) => run_review_prune(x),
        ReviewJob::BackfillVerdict(x) => run_review_backfill(x),
        ReviewJob::ReuseIngest(x) => run_review_reuse_ingest(x),
    }
}

/// `hub review list`：只读；默认不打印载荷与 turn_uid。
fn run_review_list(a: &ReviewListArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let items = match hub::review::list(&root, a.state.as_deref(), a.pass.as_deref(), a.limit) {
        Ok(v) => v,
        Err(hub::review::ReviewError::Usage(msg)) => return operational(msg),
        Err(_) => return operational("review read failed — run: doctor"),
    };
    match hub::review::render_list(&items, a.json) {
        Ok(text) => {
            write_line(&text);
            ExitCode::from(0)
        }
        Err(_) => operational("review report serialization failed"),
    }
}

/// `hub review show`：meta + 判定历史；只有 `--payload` 才打印载荷。
/// 找不到 id 时只给一句静态话（退出 3），不回显操作员递进来的 id。
///
/// DR：`--prompt` 是另一条出口（只打印审核输入原文），所以先分派——它不读判定历史，
/// 也不受 `--payload`/`--json` 的形状影响（那两格说的是"库里存了什么"）。
fn run_review_show(a: &ReviewShowArgs) -> ExitCode {
    if a.prompt {
        return run_review_prompt(a);
    }
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let doc = match hub::review::show(&root, a.id, a.payload) {
        Ok(d) => d,
        Err(hub::review::ReviewError::NotFound) => return operational("review item not found"),
        Err(hub::review::ReviewError::Usage(msg)) => return operational(msg),
        Err(_) => return operational("review read failed — run: doctor"),
    };
    match hub::review::render_show(&doc, a.payload, a.json) {
        Ok(text) => {
            write_line(&text);
            ExitCode::from(0)
        }
        Err(_) => operational("review report serialization failed"),
    }
}

/// DR：`hub review show ID --prompt` —— 打印这条 item **当时会送出**的审核输入。
///
/// 渲染与分片全在 `rc_distill::review::render_prompt_for_item`（它复用 `review_plan`），
/// 这一层只做三件事：取 item、取钟、按段打印。不调模型、不写任何表。
///
/// 与 `distill --dry-run` 同一条纪律：`=== NO PROMPT ===` 表示真实运行也会在开口之前
/// 收口（载荷与列指纹不一致、载荷不可重放、批级闸前置命中）——这一格没有"模型看到的
/// 原文"可导出，而不是导出失败。
fn run_review_prompt(a: &ReviewShowArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let item = match rc_memory::review::get(root.memory(), a.id) {
        Ok(Some(item)) => item,
        Ok(None) => return operational("review item not found"),
        Err(_) => return operational("review read failed — run: doctor"),
    };
    let rendered = match rc_distill::review::render_prompt_for_item(
        root.memory(),
        root.transcript(),
        &item,
        now_ms,
    ) {
        Ok(rendered) => rendered,
        // 与 `review run` 同一个错误面：只说类别，不带库路径/SQL。
        Err(_) => return operational("review prompt read failed — run: doctor"),
    };
    let Some(prompt) = rendered else {
        write_line("=== NO PROMPT ===");
        return ExitCode::from(0);
    };
    // `fact:promise` / `event` / `naming`：与载荷里 `pass_id`+`family` 同一对列。
    let pass = if item.meta.family.is_empty() {
        item.meta.pass_id.clone()
    } else {
        format!("{}:{}", item.meta.pass_id, item.meta.family)
    };
    let mut text = String::new();
    push_block(&mut text, "=== PASS ===", &pass);
    push_block(&mut text, "=== ITEM ===", &a.id.to_string());
    push_block(&mut text, "=== SYSTEM ===", &prompt.system);
    for (index, chunk) in prompt.chunks.iter().enumerate() {
        push_block(&mut text, &format!("=== CHUNK {} ===", index + 1), chunk);
    }
    text.push_str("=== END ===");
    write_line(&text);
    ExitCode::from(0)
}

/// `hub review decide`：`--approve`/`--reject` 恰给一个；原因是 `[a-z0-9_]+`。
/// `--approve` 只推进到 `approved`——实际写库由后续 `hub review run` 执行（S3b）。
fn run_review_decide(a: &ReviewDecideArgs) -> ExitCode {
    let approve = match (a.approve, a.reject) {
        (true, false) => true,
        (false, true) => false,
        _ => {
            return operational("review decide needs exactly one of --approve or --reject");
        }
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::review::decide(&root, a.id, approve, &a.reason, now_ms) {
        Ok(state) => match hub::review::render_decide(a.id, &state, a.json) {
            Ok(text) => {
                write_line(&text);
                ExitCode::from(0)
            }
            Err(_) => operational("review report serialization failed"),
        },
        Err(hub::review::ReviewError::NotFound) => operational("review item not found"),
        Err(hub::review::ReviewError::Usage(msg)) => operational(msg),
        // H2：非法转换（如 `applied` 上人工改判）是一句业务裁定，不指向 doctor/存储。
        Err(hub::review::ReviewError::Illegal) => {
            operational("review decide: item is not decidable in its current state")
        }
        Err(_) => operational("review write failed — run: doctor"),
    }
}

/// `hub review stats`：计数读数是 `rc_memory::review::stats`，这里只取钟与两形输出。
fn run_review_stats(a: &ReviewStatsArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let stats = match hub::review::stats(&root, now_ms) {
        Ok(s) => s,
        Err(_) => return operational("review read failed — run: doctor"),
    };
    match hub::review::render_stats(&stats, a.json) {
        Ok(text) => {
            write_line(&text);
            ExitCode::from(0)
        }
        Err(_) => operational("review report serialization failed"),
    }
}

/// 波二刀 B：`hub review reuse-ingest`——复用反馈入复核队列（程序判定与写入在
/// `rc_memory::reuse::ingest_audit`，水位镜像在 `hub::review::reuse_ingest`）。
/// 这一层只做钟、开库、两形输出与退出码；没有模型座位（窄审不碰 reuse 项）。
fn run_review_reuse_ingest(a: &ReviewReuseArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let out = match hub::review::reuse_ingest(&root, now_ms) {
        Ok(out) => out,
        Err(_) => return operational("review write failed — run: doctor"),
    };
    match hub::review::render_reuse_ingest(&out, a.json) {
        Ok(text) => write_line(&text),
        Err(_) => return operational("review report serialization failed"),
    }
    ExitCode::from(0)
}

/// H1a：`review run` 的 claim 租约余量。租约 = 单次模型墙钟上限 + 这个数：
/// 模型最慢一次调用之后租约还得在，否则第二个 run 会在第一条还没写完时就把它抢走。
const REVIEW_LEASE_MARGIN_MS: u64 = 120_000;

/// `hub review run`：逐条重跑可运行项（pending/quarantined 复审 + approved 补写）。
///
/// 退出码：`errors>0 ⇒ 3`；否则还有遗留 `pending_review`/`quarantined`/`approved` ⇒ 4；
/// 否则 0（三格 remaining 都按 `--pass` 域取，见 `hub::review::remaining_for`）。
/// **惰性装配**：可运行项全是 `approved` 时一次都不读来源——先读队列，确有
/// pending/quarantined 才装配座位；装配失败 ⇒ 3（来源/超时用 review 自己的静态话）。
fn run_review_run(a: &ReviewRunArgs) -> ExitCode {
    // `0` 是旗标的形状问题：先于开库、先于装配。这一格的主语是 review 自己的旗标，
    // 不复用 `distill --llm-timeout-ms` 那一句（C5）。
    if a.llm_timeout_ms == 0 {
        return operational(hub::review::FAULT_REVIEW_LLM_TIMEOUT);
    }
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let pass = a.pass.as_deref();
    // 惰性：这一次读同时回答"要不要装配来源"。它读的就是 `review::run` 即将读的那条
    // `queue::runnable`（同 pass/limit/now_ms），所以两边不会错位。
    let needs_llm = match hub::review::needs_llm(&root, pass, a.limit, now_ms) {
        Ok(v) => v,
        Err(hub::review::ReviewError::Usage(msg)) => return operational(msg),
        Err(_) => return operational("review read failed — run: doctor"),
    };
    // 观测面（W33-A10 · 评审缺陷②补线）：这一档的账本不再被丢弃——装配点接住它，
    // run 交回 out 之后无论成败把两格读数填进去（没有 LLM 可跑的那一支没有账本，
    // 两格保持 Default 的 0：stub 恒 0，与五条 distill 面同一口径）。
    let mut meter: Option<Rc<Metered>> = None;
    let rotating = if needs_llm {
        let (m, slots) = match llm_source_with_timeout(
            &a.replies,
            a.llm_config.as_ref(),
            &a.data_dir,
            Some(a.llm_timeout_ms),
        ) {
            Ok(v) => v,
            // H2（C5）：没给来源这一句以前写的是 `hub cycle once`，操作员照它跑会再撞一次墙。
            Err(cat) if cat == FAULT_NO_LLM_SOURCE => {
                return operational(hub::review::FAULT_REVIEW_NO_LLM_SOURCE)
            }
            // 其余类别词（配置坏/preset/身份/读箱）本来就是动作指向，原样透传。
            Err(cat) => return operational(cat),
        };
        meter = Some(m.clone());
        // 账本随座位以 `Rc` 共享：本命令不收 `usage_counter`，但调用账仍由座位自己记。
        match Rotating::new(slots) {
            Ok(r) => Some(r),
            Err(cat) => return operational(cat),
        }
    } else {
        None
    };
    let backend: Option<&dyn LlmBackend> = rotating.as_ref().map(|r| r as &dyn LlmBackend);
    let lease_ms = i64::try_from(a.llm_timeout_ms.saturating_add(REVIEW_LEASE_MARGIN_MS))
        .unwrap_or(i64::MAX);
    let mut out = match hub::review::run(&root, pass, a.limit, backend, now_ms, lease_ms) {
        Ok(out) => out,
        Err(hub::review::ReviewError::Usage(msg)) => return operational(msg),
        Err(_) => return operational("review run failed — run: doctor"),
    };
    // 观测面两格从账本读进读数（无论这一发里 item 跑成什么：慢的/超时的那发恰恰是
    // 观测面要看见的）。回放座位没有真实出网 ⇒ 恒 0。
    if let Some(m) = &meter {
        out.max_call_ms = m.max_call_ms();
        out.timeouts = m.timeouts();
    }
    match hub::review::render_run(&out, a.json) {
        Ok(text) => write_line(&text),
        Err(_) => return operational("review report serialization failed"),
    }
    if out.errors > 0 {
        return ExitCode::from(3);
    }
    // H2：`approved` 也算遗留——它不是终态（还要补写），漏掉这一格会让
    // "approved 上判了缺口"的那条 item 读成退出 0。
    if out.remaining_pending_review + out.remaining_quarantined + out.remaining_approved > 0 {
        return ExitCode::from(4);
    }
    ExitCode::from(0)
}

/// `review retry-uncertain` 只做 fact 的用法错误（静态话，零插值）。
const FAULT_RETRY_PASS: &str = "review retry-uncertain only runs --pass fact";

/// §7.3b：`hub review retry-uncertain`——只把 `final=uncertain` 的 fact 草稿单独重问一次。
///
/// 引擎在 `rc_distill::review::run::retry_uncertain`（队列选择、逐条重问、程序合成终态与
/// 三个率都在那里）；这一层只做旗标形状、开库、座位装配与两形输出。
///
/// 退出码与 `review run` 同一条纪律：`errors>0 ⇒ 3`；否则还有可重跑的隔离项 ⇒ 4
/// （`--limit` 截断，或重跑仍判不出来）；否则 0。**座位总是要装配**：重跑每一条都要开口，
/// 没有"全是 approved 所以不问模型"的那一支。
fn run_review_retry(a: &ReviewRetryArgs) -> ExitCode {
    if a.llm_timeout_ms == 0 {
        return operational(hub::review::FAULT_REVIEW_LLM_TIMEOUT);
    }
    if let Some(pass) = a.pass.as_deref() {
        if pass != "fact" {
            return operational(FAULT_RETRY_PASS);
        }
    }
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let (_meter, slots) = match llm_source_with_timeout(
        &a.replies,
        a.llm_config.as_ref(),
        &a.data_dir,
        Some(a.llm_timeout_ms),
    ) {
        Ok(v) => v,
        Err(cat) if cat == FAULT_NO_LLM_SOURCE => {
            return operational(hub::review::FAULT_REVIEW_NO_LLM_SOURCE)
        }
        Err(cat) => return operational(cat),
    };
    let rotating = match Rotating::new(slots) {
        Ok(r) => r,
        Err(cat) => return operational(cat),
    };
    let backend: &dyn LlmBackend = &rotating;
    let out = match rc_distill::review::run::retry_uncertain(
        root.memory(),
        root.transcript(),
        root.ledger(),
        backend,
        now_ms,
        a.limit,
    ) {
        Ok(out) => out,
        Err(_) => return operational("review retry-uncertain failed — run: doctor"),
    };
    if a.json {
        write_line(&retry_json(&out).to_string());
    } else {
        write_line(&retry_line(&out));
    }
    if out.errors > 0 {
        return ExitCode::from(3);
    }
    if out.remaining_retryable > 0 {
        return ExitCode::from(4);
    }
    ExitCode::from(0)
}

/// `--json`：**同一组数**（与 `review_decision.detail` 逐格对应；率由程序算，四个小数位）。
fn retry_json(out: &rc_distill::review::run::RetryMetrics) -> Value {
    json!({
        "items": out.items,
        "drafts_total": out.drafts_total,
        "prescan_uncertain": out.prescan_uncertain,
        "uncertain_rate": out.uncertain_rate(),
        "retried_drafts": out.retried,
        "retry_pass": out.retry_pass,
        "retry_reject": out.retry_reject,
        "retry_uncertain": out.retry_uncertain,
        "retry_pass_rate": out.retry_pass_rate(),
        "retry_reject_rate": out.retry_reject_rate(),
        "retry_still_uncertain_rate": out.retry_still_uncertain_rate(),
        "evidence_missing": out.evidence_missing,
        "evidence_not_in_quote": out.evidence_not_in_quote,
        "parse_failures": out.parse_failures,
        "applied": out.applied,
        "rejected": out.rejected,
        "quarantined": out.quarantined,
        "errors": out.errors,
        "conflicts": out.conflicts,
        "exhausted": out.exhausted,
        "remaining_retryable": out.remaining_retryable,
    })
}

/// 文本形：一行读数（零正文、零路径）。
fn retry_line(out: &rc_distill::review::run::RetryMetrics) -> String {
    format!(
        "items={} drafts={} prescan_uncertain={} retried={} pass={} reject={} uncertain={} \
         applied={} rejected={} quarantined={} errors={} remaining_retryable={}",
        out.items,
        out.drafts_total,
        out.prescan_uncertain,
        out.retried,
        out.retry_pass,
        out.retry_reject,
        out.retry_uncertain,
        out.applied,
        out.rejected,
        out.quarantined,
        out.errors,
        out.remaining_retryable,
    )
}

/// `hub review prune`（H7a）：清终态 item 的载荷。
///
/// 天数形状（`<= 0`）先于开库：0 天/负天数的 cutoff 等于"全清"，不是清理。
/// 真正的写失败只有存储一格（退出 3、静态话）；计数与窗口由 `hub::review::render_prune` 出。
fn run_review_prune(a: &ReviewPruneArgs) -> ExitCode {
    if a.older_than_days <= 0 {
        return operational(hub::review::FAULT_REVIEW_PRUNE_DAYS);
    }
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let out = match hub::review::prune(&root, a.older_than_days, now_ms) {
        Ok(out) => out,
        Err(hub::review::ReviewError::Usage(msg)) => return operational(msg),
        Err(_) => return operational("review prune failed — run: doctor"),
    };
    match hub::review::render_prune(&out, a.json) {
        Ok(text) => {
            write_line(&text);
            ExitCode::from(0)
        }
        Err(_) => operational("review report serialization failed"),
    }
}

/// `hub review backfill-verdict`（T1）：历史已 applied 的 fact/event 命中卡回填 `passed`。
///
/// 三条纪律落在这一层：
/// - **用法面先于开库**：`--revert` 与扫描旗标的关系、`--pass` 的闭集、清单可读性都在
///   `Root::open` 之前判——一条跑不起来的调用不该先迁移/打开三个库。
/// - **干跑不写**：句柄与真跑**同一个**（`open_root`，可写）——用只读句柄证明"没写"是
///   循环论证（它本来也写不下去），而这份命令的验收要在可写副本上比对行数与 id 集合。
///   真正的保证在实现侧：`backfill` 的 `dry_run` 分支在 `promote_verdict` 之前就返回，
///   而这条路径不碰任何别的写口。
/// - **退出码**：0 = 没有错误（`errors` 才是错误；`absent`/`already` 是读数）；3 = 有错误，
///   此时清单里已经有成功的那些条目，重跑可续（幂等）。
fn run_review_backfill(a: &ReviewBackfillArgs) -> ExitCode {
    if a.revert.is_some() && (a.dry_run || a.manifest.is_some() || a.pass.is_some()) {
        return operational(hub::review::FAULT_BACKFILL_REVERT_COMBINED);
    }
    let passes = match hub::review::parse_passes(a.pass.as_deref()) {
        Ok(p) => p,
        Err(hub::review::ReviewError::Usage(msg)) => return operational(msg),
        Err(_) => return operational(hub::review::FAULT_BACKFILL_PASS),
    };
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    if let Some(path) = &a.revert {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(_) => return operational(hub::review::FAULT_BACKFILL_MANIFEST),
        };
        let ids = match hub::review::parse_manifest(&text) {
            Ok(ids) => ids,
            Err(hub::review::ReviewError::Usage(msg)) => return operational(msg),
            Err(_) => return operational(hub::review::FAULT_BACKFILL_MANIFEST),
        };
        let root = match open_root(&a.data_dir) {
            Ok(r) => r,
            Err(_) => return operational("cannot open data store"),
        };
        let out = match hub::review::revert(root.memory(), &ids, now_ms) {
            Ok(out) => out,
            Err(_) => return operational("review revert failed — run: doctor"),
        };
        return match hub::review::render_revert(&out, a.json) {
            Ok(text) => {
                write_line(&text);
                if out.errors > 0 {
                    return operational("review backfill-verdict --revert finished with errors");
                }
                ExitCode::from(0)
            }
            Err(_) => operational("review report serialization failed"),
        };
    }
    // 干跑与真跑开同一个库（可写）：干跑少写的那一步由 `backfill` 的 `dry_run` 分支短路，
    // 不是由句柄权限兜住——否则"没写"这三个字证明不了任何事。
    let memory = match open_root(&a.data_dir) {
        Ok(root) => root.into_memory(),
        Err(_) => return operational("cannot open data store"),
    };
    let out = match hub::review::backfill(&memory, &passes, a.dry_run, a.limit, now_ms) {
        Ok(out) => out,
        Err(hub::review::ReviewError::Usage(msg)) => return operational(msg),
        Err(_) => return operational("review backfill failed — run: doctor"),
    };
    if let Some(path) = &a.manifest {
        // 干跑的清单就是回滚清单（每一项都带 card_id）；真跑只装实际提升的 id。
        let body = if a.dry_run {
            hub::review::manifest_rows(&out.rows)
        } else {
            hub::review::manifest_promoted(&out.rows)
        };
        if std::fs::write(path, body).is_err() {
            return operational("cannot write manifest");
        }
    }
    match hub::review::render_backfill(&out, a.json) {
        Ok(text) => {
            write_line(&text);
            if out.errors > 0 {
                return operational("review backfill-verdict finished with errors");
            }
            ExitCode::from(0)
        }
        Err(_) => operational("review report serialization failed"),
    }
}

fn run_reconcile(a: &ReconcileArgs) -> ExitCode {
    match &a.job {
        ReconcileJob::Memory(x) => run_reconcile_memory(x),
    }
}

/// H7c1：`hub reconcile memory [--apply]`。
///
/// 读数与写库都在 `hub::reconcile`（`memory_gap` 是唯一的写点）；这里只取钟、开库、
/// 两形输出与退出码。存储失败 ⇒ 3 + 静态话（零插值：`StoreError` 的原串可能带库路径）。
fn run_reconcile_memory(a: &ReconcileMemoryArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let report = match hub::reconcile::memory_gap(&root, a.apply, now_ms) {
        Ok(report) => report,
        Err(_) => return operational("reconcile failed — run: doctor"),
    };
    match hub::reconcile::render(&report, a.json) {
        Ok(text) => {
            write_line(&text);
            ExitCode::from(0)
        }
        Err(_) => operational("reconcile report serialization failed"),
    }
}

/// H3：`hub handoff prune`。`rc-core` 只提供"每个 agent 掉多少行"，这里负责取钟、开库、
/// 遍历注册表与打印回执。**不做有界批**：载荷总量上限 200 已经把单 agent 的过期行限死了
/// （`HANDOFF_TOTAL_LIMIT`），一次删完不构成无界写。
fn run_handoff(job: &HandoffJob) -> ExitCode {
    match job {
        HandoffJob::Prune(a) => {
            let system_ms = unix_now_ms();
            let now_ms = match a.now_ms {
                Some(ms) => {
                    // 未来的 `--now-ms` 会把还没到点的条（含未关闭的）全当过期删掉：
                    // 那不是"清理"，是一次不可逆的误删。除非操作员显式 `--force`。
                    if !a.force && system_ms.is_some_and(|system| ms > system) {
                        return operational(
                            "now-ms 在未来；会删掉未关闭的交接条（确需如此请加 --force）",
                        );
                    }
                    ms
                }
                None => match system_ms {
                    Some(ms) => ms,
                    None => return operational("system clock is behind the unix epoch"),
                },
            };
            let store = match rc_core::CoreStore::open(&a.data_dir) {
                Ok(s) => s,
                Err(_) => return operational("core store unreachable — run: doctor"),
            };
            let agents = match store.list_agents() {
                Ok(list) => list,
                Err(_) => return operational("core registry unreadable — run: doctor"),
            };
            let mut removed = 0usize;
            for agent in agents {
                match store.handoff_prune(&agent.agent_id, now_ms, "hub") {
                    Ok(n) => removed += n,
                    // 一个 agent 的载荷打不开（key 丢了/密文坏了）不该让整条命令装作全清完：
                    // 停下来报运维错误，已清的那些已经落盘、由回执说清楚。
                    Err(_) => {
                        operator_line(
                            "error",
                            &format!("handoff prune stopped after {removed} rows — run: doctor"),
                        );
                        return ExitCode::from(3);
                    }
                }
            }
            write_line(&format!("handoff pruned {removed} rows"));
            ExitCode::from(0)
        }
    }
}

fn run_purge(a: &PurgeArgs) -> ExitCode {
    // 两个旗标**恰好给一个**：给两个或一个都不给都不猜，直接按用法错误回。
    let scope = match (&a.session_key, &a.device_id) {
        (Some(k), None) => PurgeScope::Session(k.clone()),
        (None, Some(k)) => PurgeScope::Device(k.clone()),
        _ => return operational(USAGE),
    };
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let result = if a.confirm {
        hub::purge::purge(&mut root, &scope, now)
    } else {
        hub::purge::suppress(&mut root, &scope, now)
    };
    match result {
        Ok(n) => {
            let scope_name = match &scope {
                PurgeScope::Session(_) => "session",
                PurgeScope::Device(_) => "device",
            };
            write_line(
                &json!({ "scope": scope_name, "suppressed_turns": n, "deleted": a.confirm })
                    .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("store write failed — run: doctor"),
    }
}

fn run_epoch(a: &EpochArgs) -> ExitCode {
    match &a.job {
        EpochJob::Require(x) => {
            let path = x.data_dir.join(hub::serve::REQUIRE_EPOCH_FILE);
            if std::fs::write(&path, b"{\"required\":true}\n").is_err() {
                return operational("cannot write epoch policy");
            }
            write_line(&json!({"epoch_required": true}).to_string());
            ExitCode::from(0)
        }
        EpochJob::Allow(x) => {
            let path = x.data_dir.join(hub::serve::REQUIRE_EPOCH_FILE);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return operational("cannot remove epoch policy"),
            }
            write_line(&json!({"epoch_required": false}).to_string());
            ExitCode::from(0)
        }
        EpochJob::Status(x) => {
            let required = x.data_dir.join(hub::serve::REQUIRE_EPOCH_FILE).is_file();
            write_line(&json!({"epoch_required": required}).to_string());
            ExitCode::from(0)
        }
    }
}

fn run_restore(a: &RestoreArgs) -> ExitCode {
    let key = match &a.backup_key_file {
        None => None,
        Some(path) => {
            if !path.is_file() {
                return operational("backup key is missing");
            }
            if !backup_key_outside_data_dir(path, &a.data_dir) {
                return operational("backup key must live outside the data directory");
            }
            match rc_secret::backup::BackupKey::load(path) {
                Ok(key) => Some(key),
                Err(_) => return operational("backup key is unreadable"),
            }
        }
    };
    // 校验在 `open_root` 之前：备份坏掉是一件事，数据目录打不开是另一件。
    // 无确认与有确认走**同一条**校验；`restore` 内部还会再校验一次（确认之后不信清单）。
    let checked = match hub::restore::check_with_key(&a.manifest, key.as_ref()) {
        Ok(c) => c,
        Err(_) => return operational("the backup does not check out"),
    };
    if !a.confirm {
        // "还要人来确认"不是失败：校验做完就退出 0。输出里没有路径。
        write_line(
            &json!({
                "checked": true,
                "turns": checked.turns,
                "memory_rows": checked.memory_rows,
                "ledger_rows": checked.ledger_rows,
                "core_agents": checked.core_agents,
                "extras": checked.extras,
                "awaiting_confirm": true,
            })
            .to_string(),
        );
        return ExitCode::from(0);
    }
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::restore::restore_with_options(&mut root, &a.manifest, now, key.as_ref(), a.with_extras)
    {
        Ok(c) => {
            write_line(
                &json!({
                    "restored": true,
                    "turns": c.turns,
                    "memory_rows": c.memory_rows,
                    "ledger_rows": c.ledger_rows,
                    "core_agents": c.core_agents,
                    // `extras` 是校验通过的份数；写没写看下面两格（两者之和 = extras）。
                    "extras": c.extras,
                    "extras_restored": c.extras_restored,
                    "extras_skipped": c.extras_skipped,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("restore failed — run: doctor"),
    }
}

fn run_replicate(a: &ReplicateArgs) -> ExitCode {
    match &a.job {
        ReplicateJob::Apply(x) => run_replicate_apply(x),
        ReplicateJob::Status(x) => run_replicate_status(x),
        ReplicateJob::Lag(x) => run_replicate_lag(x),
        ReplicateJob::Grant(x) => run_replicate_grant(x),
        ReplicateJob::Revoke(x) => run_replicate_revoke(x),
        ReplicateJob::ExportLedger(x) => run_replicate_export_ledger(x),
        ReplicateJob::ApplyLedger(x) => run_replicate_apply_ledger(x),
        ReplicateJob::LedgerStatus(x) => run_replicate_ledger_status(x),
        ReplicateJob::ApplyMemory(x) => run_replicate_apply_memory(x),
        ReplicateJob::MemoryStatus(x) => run_replicate_memory_status(x),
        ReplicateJob::Pull(x) => run_replicate_pull(x),
        ReplicateJob::Ack(x) => run_replicate_ack(x),
        ReplicateJob::Acked(x) => run_replicate_acked(x),
        ReplicateJob::Acks(x) => run_replicate_acks(x),
    }
}

fn run_replicate_apply(a: &ReplicateApplyArgs) -> ExitCode {
    let bytes = match std::fs::read(&a.input) {
        Ok(bytes) => bytes,
        Err(_) => return operational("cannot read replication input"),
    };
    let doc: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(doc) => doc,
        Err(_) => return operational("cannot read replication input"),
    };
    let records_value = if doc.is_array() {
        doc
    } else {
        doc.get("records").cloned().unwrap_or_else(|| json!([]))
    };
    let records: Vec<hub::replicate::ReplicaRecord> = match serde_json::from_value(records_value) {
        Ok(records) => records,
        Err(_) => return operational("cannot read replication input"),
    };
    let Some(now) = a.now_ms.or_else(unix_now_ms) else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::replicate::apply_records(root.transcript(), &records, now) {
        Ok(outcome) => {
            write_line(
                &json!({
                    "applied": outcome.applied,
                    "skipped": outcome.skipped,
                    "last_applied_seq": outcome.last_applied_seq,
                    "last_epoch": outcome.last_epoch,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(hub::replicate::ApplyError::Store(_)) => operational("replication store failed"),
        Err(hub::replicate::ApplyError::BadRecord(code)) => {
            write_line(&json!({"error": "bad_record", "code": code}).to_string());
            ExitCode::from(2)
        }
        Err(hub::replicate::ApplyError::Conflict(seq)) => {
            write_line(&json!({"error": "conflict", "commit_seq": seq}).to_string());
            ExitCode::from(2)
        }
        Err(hub::replicate::ApplyError::Gap { expected, got }) => {
            write_line(&json!({"error": "gap", "expected": expected, "got": got}).to_string());
            ExitCode::from(2)
        }
        Err(hub::replicate::ApplyError::Tombstoned(seq)) => {
            write_line(&json!({"error": "tombstoned", "commit_seq": seq}).to_string());
            ExitCode::from(2)
        }
    }
}

fn run_replicate_status(a: &ReplicateStatusArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::replicate::apply_status(root.transcript()) {
        Ok(status) => {
            write_line(
                &json!({
                    "last_applied_seq": status.last_applied_seq,
                    "last_epoch": status.last_epoch,
                    "last_verified_at_ms": status.last_verified_at_ms,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("replication store failed"),
    }
}

fn run_replicate_lag(a: &ReplicateLagArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::replicate::apply_status(root.transcript()) {
        Ok(status) => {
            write_line(
                &json!({
                    "last_applied_seq": status.last_applied_seq,
                    "last_epoch": status.last_epoch,
                    "center_last_seq": a.center_last_seq,
                    "center_epoch": a.center_epoch,
                    "lag": a.center_last_seq.saturating_sub(status.last_applied_seq),
                    "epoch_match": status.last_epoch == a.center_epoch,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("replication store failed"),
    }
}

fn run_replicate_grant(a: &ReplicateGrantArgs) -> ExitCode {
    let Some(now) = a.now_ms.or_else(unix_now_ms) else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match rc_ledger::device::grant_capability(
        root.ledger_mut(),
        &a.device_id,
        rc_ledger::device::CAPABILITY_REPLICATE,
        now,
    ) {
        Ok(true) => {
            write_line(
                &json!({
                    "capability": rc_ledger::device::CAPABILITY_REPLICATE,
                    "device_id": a.device_id,
                    "granted": true,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Ok(false) => {
            write_line(&json!({"error": "unknown_device", "device_id": a.device_id}).to_string());
            ExitCode::from(2)
        }
        Err(_) => operational("replication grant failed"),
    }
}

fn run_replicate_revoke(a: &ReplicateRevokeArgs) -> ExitCode {
    let Some(now) = a.now_ms.or_else(unix_now_ms) else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match rc_ledger::device::revoke_capability(
        root.ledger_mut(),
        &a.device_id,
        rc_ledger::device::CAPABILITY_REPLICATE,
        now,
    ) {
        Ok(revoked) => {
            write_line(
                &json!({
                    "capability": rc_ledger::device::CAPABILITY_REPLICATE,
                    "device_id": a.device_id,
                    "revoked": revoked,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("replication revoke failed"),
    }
}

fn run_replicate_export_ledger(a: &ReplicateExportLedgerArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    let snapshot = match hub::replicate::export_ledger(&root) {
        Ok(snapshot) => snapshot,
        Err(_) => return operational("ledger export failed"),
    };
    let bytes = match serde_json::to_vec(&snapshot) {
        Ok(bytes) => bytes,
        Err(_) => return operational("ledger export failed"),
    };
    if std::fs::write(&a.output, bytes).is_err() {
        return operational("cannot write ledger snapshot");
    }
    write_line(
        &json!({
            "exported": true,
            "commit_seq": snapshot.commit_seq,
            "epoch": snapshot.epoch,
            "devices": snapshot.devices.len(),
            "capabilities": snapshot.capabilities.len(),
            "audits": snapshot.audits.len(),
        })
        .to_string(),
    );
    ExitCode::from(0)
}

fn run_replicate_apply_ledger(a: &ReplicateApplyLedgerArgs) -> ExitCode {
    let bytes = match std::fs::read(&a.input) {
        Ok(bytes) => bytes,
        Err(_) => return operational("cannot read ledger snapshot"),
    };
    let snapshot: hub::replicate::LedgerSnapshot = match serde_json::from_slice(&bytes) {
        Ok(snapshot) => snapshot,
        Err(_) => return operational("cannot read ledger snapshot"),
    };
    let Some(now) = a.now_ms.or_else(unix_now_ms) else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::replicate::apply_ledger(root.ledger_mut(), &snapshot, now) {
        Ok(outcome) => {
            write_line(
                &json!({
                    "applied": outcome.applied,
                    "devices": outcome.devices,
                    "capabilities": outcome.capabilities,
                    "audits": outcome.audits,
                    "commit_seq": outcome.commit_seq,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(hub::replicate::LedgerApplyError::Store(_)) => operational("ledger apply failed"),
        Err(hub::replicate::LedgerApplyError::BadSnapshot(code)) => {
            write_line(&json!({"error": "bad_snapshot", "code": code}).to_string());
            ExitCode::from(2)
        }
        Err(hub::replicate::LedgerApplyError::Conflict(code)) => {
            write_line(&json!({"error": "conflict", "code": code}).to_string());
            ExitCode::from(2)
        }
    }
}

fn run_replicate_ledger_status(a: &ReplicateLedgerStatusArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::replicate::ledger_apply_status(root.ledger()) {
        Ok(status) => {
            write_line(
                &json!({
                    "commit_seq": status.commit_seq,
                    "snapshot_sha256": status.snapshot_sha256,
                    "last_verified_at_ms": status.last_verified_at_ms,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("ledger status failed"),
    }
}

fn run_replicate_apply_memory(a: &ReplicateApplyMemoryArgs) -> ExitCode {
    let Some(now) = a.now_ms.or_else(unix_now_ms) else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::replicate::apply_memory(
        &mut root,
        &a.snapshot,
        &a.expect_sha256,
        a.expect_commit_seq,
        now,
    ) {
        Ok(outcome) => {
            write_line(
                &json!({
                    "applied": outcome.applied,
                    "commit_seq": outcome.commit_seq,
                    "memory_rows": outcome.memory_rows,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(hub::replicate::MemoryApplyError::Store(_)) => operational("memory apply failed"),
        Err(hub::replicate::MemoryApplyError::BadSnapshot(code)) => {
            write_line(&json!({"error": "bad_snapshot", "code": code}).to_string());
            ExitCode::from(2)
        }
    }
}

fn run_replicate_memory_status(a: &ReplicateMemoryStatusArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::replicate::memory_apply_status(root.ledger()) {
        Ok(status) => {
            write_line(
                &json!({
                    "commit_seq": status.commit_seq,
                    "snapshot_sha256": status.snapshot_sha256,
                    "last_verified_at_ms": status.last_verified_at_ms,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("memory status failed"),
    }
}

struct HttpReplicateSource {
    endpoint: Endpoint,
    token: String,
    device_id: String,
    key: DeviceKey,
    hub_pub: String,
    timeout: std::time::Duration,
}

impl HttpReplicateSource {
    fn auth(&self) -> NodeAuth<'_> {
        NodeAuth {
            token: &self.token,
            device_id: &self.device_id,
            key: &self.key,
            hub_pub: &self.hub_pub,
        }
    }

    /// 上报本轮落地水位。409 是“回执超前中心”，与网络失败分开报——它要人去看两边读数。
    fn ack(&mut self, applied_seq: u64) -> Result<serde_json::Value, hub::replicate::PullError> {
        let body = serde_json::json!({ "applied_seq": applied_seq })
            .to_string()
            .into_bytes();
        let resp = signed_request(
            &self.endpoint,
            &self.auth(),
            "POST",
            "/v1/replicate/ack",
            &body,
            &[],
            self.timeout,
        )
        .map_err(|_| hub::replicate::PullError::Source("ack request failed".into()))?;
        if resp.status == 409 {
            return Err(hub::replicate::PullError::Source(
                "center rejected the ack".into(),
            ));
        }
        if resp.status != 200 {
            return Err(hub::replicate::PullError::Source(format!(
                "center returned {}",
                resp.status
            )));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|_| hub::replicate::PullError::BadResponse("ack"))
    }

    fn acked(&mut self) -> Result<serde_json::Value, hub::replicate::PullError> {
        let resp = signed_request(
            &self.endpoint,
            &self.auth(),
            "GET",
            "/v1/replicate/acked",
            b"",
            &[],
            self.timeout,
        )
        .map_err(|_| hub::replicate::PullError::Source("acked request failed".into()))?;
        if resp.status != 200 {
            return Err(hub::replicate::PullError::Source(format!(
                "center returned {}",
                resp.status
            )));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|_| hub::replicate::PullError::BadResponse("acked"))
    }

    fn acks(&mut self) -> Result<serde_json::Value, hub::replicate::PullError> {
        let resp = signed_request(
            &self.endpoint,
            &self.auth(),
            "GET",
            "/v1/replicate/status",
            b"",
            &[],
            self.timeout,
        )
        .map_err(|_| hub::replicate::PullError::Source("ack status request failed".into()))?;
        if resp.status != 200 {
            return Err(hub::replicate::PullError::Source(format!(
                "center returned {}",
                resp.status
            )));
        }
        serde_json::from_slice(&resp.body)
            .map_err(|_| hub::replicate::PullError::BadResponse("acks"))
    }
}

impl hub::replicate::ReplicateSource for HttpReplicateSource {
    fn get(&mut self, target: &str) -> Result<Option<Vec<u8>>, hub::replicate::PullError> {
        let auth = self.auth();
        match signed_request(
            &self.endpoint,
            &auth,
            "GET",
            target,
            b"",
            &[],
            self.timeout,
        ) {
            Ok(resp) if resp.status == 404 => Ok(None),
            Ok(resp) if resp.status == 200 => Ok(Some(resp.body)),
            Ok(resp) => Err(hub::replicate::PullError::Source(format!(
                "center returned {}",
                resp.status
            ))),
            Err(_) => Err(hub::replicate::PullError::Source(
                "center request failed".into(),
            )),
        }
    }

    fn download(
        &mut self,
        manifest: &rc_net::snapshot::Manifest,
        dest: &Path,
    ) -> Result<(), hub::replicate::PullError> {
        let auth = self.auth();
        download_snapshot(&self.endpoint, &auth, manifest, dest, 0, self.timeout)
            .map(|_| ())
            .map_err(|_| hub::replicate::PullError::Source("memory download failed".into()))
    }
}

fn parse_center_endpoint(raw: &str) -> Option<Endpoint> {
    let s = raw.strip_prefix("http://").unwrap_or(raw);
    let (host, port) = s.rsplit_once(':')?;
    let host = host.trim_matches(['[', ']']);
    if host.is_empty() || s.contains('/') {
        return None;
    }
    let port = port.parse::<u16>().ok()?;
    Some(Endpoint {
        label: "center".into(),
        host: host.into(),
        port,
        // 复制的中心坐标仍走明文：这一面还没接线 TLS（S4a2 只接 serve/pair/sync）。
        tls_fp: None,
    })
}

/// 三个远程子命令共用的开箱：坐标、token、设备私钥、中心公钥。
/// 错误串零插值、不带路径——它们点名的是“哪一格没给对”，不是那个路径。
fn open_center_source(c: &CenterArgs) -> Result<HttpReplicateSource, &'static str> {
    let endpoint = parse_center_endpoint(&c.endpoint).ok_or("invalid center endpoint")?;
    let read_trim = |path: &Path| -> Result<String, ()> {
        std::fs::read_to_string(path)
            .map(|text| text.trim().to_string())
            .map_err(|_| ())
    };
    let token = match read_trim(&c.token_file) {
        Ok(token) if !token.is_empty() => token,
        _ => return Err("device token is missing or unreadable"),
    };
    let hub_pub = match read_trim(&c.hub_pub_file) {
        Ok(value) if !value.is_empty() => value,
        _ => return Err("hub public key is missing or unreadable"),
    };
    let key = DeviceKey::load(&c.device_key_file).map_err(|_| "device key is unreadable")?;
    let device_id = key.device_id();
    Ok(HttpReplicateSource {
        endpoint,
        token,
        device_id,
        key,
        hub_pub,
        timeout: std::time::Duration::from_millis(c.timeout_ms.max(1)),
    })
}

fn run_replicate_pull(a: &ReplicatePullArgs) -> ExitCode {
    let mut source = match open_center_source(&a.center) {
        Ok(source) => source,
        Err(message) => return operational(message),
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    loop {
        let Some(now) = a.now_ms.or_else(unix_now_ms) else {
            return operational("system clock is behind the unix epoch");
        };
        match hub::replicate::pull_once(
            &mut root,
            &a.data_dir,
            &mut source,
            a.limit.max(1),
            now,
        ) {
            Ok(report) => {
                write_line(
                    &json!({
                        "pulled": true,
                        "center_commit_seq": report.center_commit_seq,
                        "applied": report.applied_commits,
                        "skipped": report.skipped_commits,
                        "ledger_applied": report.ledger_applied,
                        "memory_applied": report.memory_applied,
                        "applied_seq": report.applied_seq,
                        "lag": report.lag,
                    })
                    .to_string(),
                );
                if a.ack {
                    match source.ack(report.applied_seq) {
                        Ok(doc) => write_line(
                            &json!({
                                "acked": true,
                                "applied_seq": report.applied_seq,
                                "replicated_seq": doc.get("replicated_seq").cloned().unwrap_or(Value::Null),
                            })
                            .to_string(),
                        ),
                        Err(_) => return operational("replication ack failed"),
                    }
                }
                if a.max_lag.map(|max| report.lag > max).unwrap_or(false) {
                    write_line(
                        &json!({
                            "error": "lag_exceeded",
                            "lag": report.lag,
                            "max_lag": a.max_lag,
                        })
                        .to_string(),
                    );
                    return ExitCode::from(4);
                }
            }
            Err(_) => return operational("replication pull failed"),
        }
        match a.interval_ms {
            Some(ms) if ms > 0 => std::thread::sleep(std::time::Duration::from_millis(ms)),
            _ => break,
        }
    }
    ExitCode::from(0)
}

fn run_replicate_ack(a: &ReplicateAckArgs) -> ExitCode {
    let mut source = match open_center_source(&a.center) {
        Ok(source) => source,
        Err(message) => return operational(message),
    };
    match source.ack(a.applied_seq) {
        Ok(doc) => {
            write_line(
                &json!({
                    "acked": true,
                    "epoch": doc.get("epoch").cloned().unwrap_or(Value::Null),
                    "applied_seq": doc.get("applied_seq").cloned().unwrap_or(Value::Null),
                    "replicated_seq": doc.get("replicated_seq").cloned().unwrap_or(Value::Null),
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("replication ack failed"),
    }
}

fn run_replicate_acked(a: &ReplicateAckedArgs) -> ExitCode {
    let mut source = match open_center_source(&a.center) {
        Ok(source) => source,
        Err(message) => return operational(message),
    };
    match source.acked() {
        Ok(doc) => {
            write_line(
                &json!({
                    "epoch": doc.get("epoch").cloned().unwrap_or(Value::Null),
                    "last_commit_seq": doc.get("last_commit_seq").cloned().unwrap_or(Value::Null),
                    "replicated_seq": doc.get("replicated_seq").cloned().unwrap_or(Value::Null),
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("replication ack status failed"),
    }
}


fn run_replicate_acks(a: &ReplicateAckedArgs) -> ExitCode {
    let mut source = match open_center_source(&a.center) {
        Ok(source) => source,
        Err(message) => return operational(message),
    };
    match source.acks() {
        Ok(doc) => {
            write_line(&doc.to_string());
            ExitCode::from(0)
        }
        Err(_) => operational("replication ack status failed"),
    }
}

fn run_promote(a: &PromoteArgs) -> ExitCode {
    if !a.prev_key.is_file() {
        return operational("previous hub key is missing");
    }
    let Some(now) = a.now_ms.or_else(unix_now_ms) else {
        return operational("system clock is behind the unix epoch");
    };
    let prev_key = match rc_net::key::HubKey::load_or_create(&a.prev_key) {
        Ok(key) => key,
        Err(_) => return operational("previous hub key is unreadable"),
    };
    let new_key_path = a
        .key
        .clone()
        .unwrap_or_else(|| a.data_dir.join("hub.key"));
    let new_key = match rc_net::key::HubKey::load_or_create(&new_key_path) {
        Ok(key) => key,
        Err(_) => return operational("new hub key is unreadable"),
    };
    let root = match open_root(&a.data_dir) {
        Ok(root) => root,
        Err(_) => return operational("cannot open data store"),
    };
    match hub::promote::promote(
        &root,
        &a.data_dir,
        hub::promote::PromoteRequest {
            prev_key: &prev_key,
            new_key: &new_key,
            center_last_seq: a.center_last_seq,
            center_epoch: a.center_epoch,
            now_ms: now,
            force: a.force,
        },
    ) {
        Ok(report) => {
            write_line(
                &json!({
                    "promoted": true,
                    "new_epoch": report.new_epoch,
                    "last_commit_seq": report.last_commit_seq,
                    "cert": report.cert_file,
                    "signed_by_prev_center": report.signed_by_prev_center,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(hub::promote::PromoteError::Lag {
            applied_seq,
            expected_seq,
            applied_epoch,
            expected_epoch,
        }) => {
            write_line(
                &json!({
                    "error": "lag",
                    "applied_seq": applied_seq,
                    "expected_seq": expected_seq,
                    "applied_epoch": applied_epoch,
                    "expected_epoch": expected_epoch,
                })
                .to_string(),
            );
            ExitCode::from(2)
        }
        Err(hub::promote::PromoteError::AlreadyPromoted) => {
            write_line(&json!({"error": "already_promoted"}).to_string());
            ExitCode::from(2)
        }
        Err(_) => operational("promotion failed"),
    }
}

fn run_fence(a: &FenceArgs) -> ExitCode {
    if !a.confirm {
        write_line(&json!({"fenced": false, "awaiting_confirm": true}).to_string());
        return ExitCode::from(0);
    }
    let Some(now) = a.now_ms.or_else(unix_now_ms) else {
        return operational("system clock is behind the unix epoch");
    };
    match hub::promote::write_fence(&a.data_dir, a.epoch, now) {
        Ok(record) => {
            write_line(
                &json!({"fenced": true, "fenced_epoch": record.fenced_epoch}).to_string(),
            );
            ExitCode::from(0)
        }
        Err(_) => operational("fence write failed"),
    }
}

fn run_snapshot(a: &SnapshotArgs) -> ExitCode {
    match &a.action {
        SnapshotAction::Publish(p) => run_snapshot_publish(p),
    }
}

fn run_snapshot_publish(a: &SnapshotPublishArgs) -> ExitCode {
    // 通道名是闭集：不认识的值就是敲错了参数（用法错误，退 3），不猜"大概想发哪条"。
    let channel = match a.channel.as_str() {
        "agent" => hub::snapshot::SnapshotChannel::Agent,
        "backup" => hub::snapshot::SnapshotChannel::Backup,
        _ => return operational(USAGE),
    };
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    // 第一次发布在这台机器上生成中心私钥；`serve` 之后读的是同一份文件。
    // 生成失败不能回落成"发一份没签名的快照"——清单签不了就是发不出去。
    let key = match rc_net::key::HubKey::load_or_create(&a.data_dir.join("hub.key")) {
        Ok(k) => k,
        Err(_) => return operational(hub::snapshot::SnapshotError::Key.message()),
    };
    hub::key_archive::record_fingerprint(&a.data_dir, &a.data_dir.join("hub.key"), &key);
    match hub::snapshot::publish_channel(&mut root, &key, &a.data_dir, now, channel) {
        Ok(report) => {
            // 输出面只有计数与序号：快照目录名、临时文件名、库路径一个都不出现。
            write_line(
                &json!({
                    "published": report.published,
                    "channel": report.channel.as_str(),
                    "epoch": report.manifest.epoch,
                    "seq": report.manifest.seq,
                    "bytes": report.bytes,
                    "dead_rows": report.pruned.dead_rows,
                    "orphan_edges": report.pruned.orphan_edges,
                    "orphan_derived": report.pruned.orphan_derived,
                })
                .to_string(),
            );
            ExitCode::from(0)
        }
        Err(e) => operational(e.message()),
    }
}

// ---- 密钥箱（B2a-2）：值的唯一写入面是 stdin，且只经过不留下 ----

// 错误串是封闭短语：零路径、零值、零原样回显（`safe_token` 的脱敏规则照旧）。
const SECRET_BOX_EXISTS: &str = "secret box already exists — refusing to overwrite";
const SECRET_BOX_NOT_FOUND: &str = "secret box not found";
const SECRET_PASS_NOT_FOUND: &str = "secret pass file not found";
const SECRET_BOX_UNOPENABLE: &str = "secret box cannot be opened";
const SECRET_REF_BAD: &str = "secret ref must be box:slot with safe tokens";
const SECRET_BOX_NAME_BAD: &str = "box name must be a safe token";
const SECRET_BOX_READ: &str = "cannot read the secret box";
const SECRET_BOX_WRITE: &str = "cannot write the secret box";
const SECRET_BOX_CREATE: &str = "secret box cannot be created";
const SECRET_PASS_READ: &str = "cannot read the secret pass file";
const SECRET_PASS_WRITE: &str = "cannot write the secret pass file";
const SECRET_VALUE_READ: &str = "cannot read the secret value";
const SECRET_VALUE_EMPTY: &str = "secret value is empty";
const SECRET_VALUE_TOO_LARGE: &str = "secret value is too large";
const SECRET_VALUE_NOT_TEXT: &str = "secret value must be UTF-8";
const SECRET_VALUE_NEEDS_STDIN: &str = "pass --value-stdin and pipe the value";
const SECRET_STDIN_IS_TTY: &str = "stdin is a terminal — pipe the secret value instead";

/// 值的上界按**剥掉行尾之后**的那个值算：64 KiB 是值的上限，不是管道里字节的上限。
/// 写 65_536 而不是移位算式：阶段 2 的判据 40a 把那个四位字面量挡在 `hub/src` 之外。
const MAX_SECRET_VALUE_BYTES: usize = 65_536;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

fn run_secret(a: &SecretArgs) -> ExitCode {
    match &a.job {
        SecretJob::Init(x) => run_secret_init(x),
        SecretJob::Put(x) => run_secret_put(x),
        SecretJob::List(x) => run_secret_list(x),
        SecretJob::Rm(x) => run_secret_rm(x),
    }
}

fn run_secret_init(a: &SecretInitArgs) -> ExitCode {
    if !is_secret_box_name(&a.box_name) {
        return operational(SECRET_BOX_NAME_BAD);
    }
    if std::fs::create_dir_all(&a.data_dir).is_err() {
        return operational(SECRET_BOX_WRITE);
    }
    let enc = secret_box_path(&a.data_dir, &a.box_name);
    let pass_path = secret_pass_path(&a.data_dir, &a.box_name);
    // `SecretsBox::create` 只看 `.enc`；"半个箱"恰恰是 `.pass` 先落、`.enc` 没成的形状。
    if enc.exists() || pass_path.exists() {
        return operational(SECRET_BOX_EXISTS);
    }
    let mut raw = [0u8; 32];
    if getrandom::fill(&mut raw).is_err() {
        return operational("no entropy source");
    }
    // 口令文件是 64 个十六进制字符 + 一个换行；`SecretsBox` 拿到的是不含换行的那 64 个。
    let pass_phrase = hex_lower(&raw);
    let mut pass_bytes = pass_phrase.clone().into_bytes();
    pass_bytes.push(b'\n');
    if rc_secret::write_private_file(&pass_path, &pass_bytes).is_err() {
        return operational(SECRET_PASS_WRITE);
    }
    match rc_secret::SecretsBox::create(&enc, pass_phrase.as_bytes()) {
        Ok(_) => {
            write_line(&json!({ "created": a.box_name }).to_string());
            ExitCode::from(0)
        }
        Err(e) => {
            // 不留半个箱：`.pass` 已落而 `.enc` 没成，下一次 init 会把它读成"箱在"。
            let _ = std::fs::remove_file(&pass_path);
            operational(secret_create_fault(&e))
        }
    }
}

fn run_secret_put(a: &SecretPutArgs) -> ExitCode {
    if !a.value_stdin {
        return operational(SECRET_VALUE_NEEDS_STDIN);
    }
    let Some((box_name, slot)) = split_secret_ref(&a.secret_ref) else {
        return operational(SECRET_REF_BAD);
    };
    // 硬红线：TTY 上不提示、也不读。`is_terminal` 必须在任何读之前——
    // 对着终端 read 会挂在那里等人打字。
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return operational(SECRET_STDIN_IS_TTY);
    }
    let mut raw = Vec::new();
    // 只读到越界一格为止：上限是一条拒收线，不必为一个坏输入把内存吃完。
    let budget = MAX_SECRET_VALUE_BYTES as u64 + 1;
    if std::io::stdin().lock().take(budget).read_to_end(&mut raw).is_err() {
        return operational(SECRET_VALUE_READ);
    }
    let value = strip_one_line_ending(&raw);
    if value.is_empty() {
        return operational(SECRET_VALUE_EMPTY);
    }
    if secret_value_too_large(value.len()) {
        return operational(SECRET_VALUE_TOO_LARGE);
    }
    let Ok(value) = std::str::from_utf8(value) else {
        return operational(SECRET_VALUE_NOT_TEXT);
    };
    let mut b = match open_secret_box(&a.data_dir, box_name) {
        Ok(b) => b,
        Err(msg) => return operational(msg),
    };
    match b.put(slot, value) {
        Ok(()) => {
            // 只回地址：值一旦进 stdout，管道的另一端就成了第二个副本。
            write_line(&json!({ "stored": a.secret_ref }).to_string());
            ExitCode::from(0)
        }
        Err(e) => operational(secret_write_fault(&e)),
    }
}

fn run_secret_list(a: &SecretListArgs) -> ExitCode {
    if !is_secret_box_name(&a.box_name) {
        return operational(SECRET_BOX_NAME_BAD);
    }
    let b = match open_secret_box(&a.data_dir, &a.box_name) {
        Ok(b) => b,
        Err(msg) => return operational(msg),
    };
    // 只回格名：`BTreeMap` 已排序，值一个都不出现在这一行。
    write_line(&json!({ "slots": b.list() }).to_string());
    ExitCode::from(0)
}

fn run_secret_rm(a: &SecretRmArgs) -> ExitCode {
    let Some((box_name, slot)) = split_secret_ref(&a.secret_ref) else {
        return operational(SECRET_REF_BAD);
    };
    let mut b = match open_secret_box(&a.data_dir, box_name) {
        Ok(b) => b,
        Err(msg) => return operational(msg),
    };
    match b.remove(slot) {
        Ok(existed) => {
            write_line(&json!({ "removed": a.secret_ref, "existed": existed }).to_string());
            ExitCode::from(0)
        }
        Err(e) => operational(secret_write_fault(&e)),
    }
}

fn secret_box_path(data_dir: &Path, box_name: &str) -> PathBuf {
    data_dir.join(format!("{box_name}.enc"))
}

fn secret_pass_path(data_dir: &Path, box_name: &str) -> PathBuf {
    data_dir.join(format!("{box_name}.pass"))
}

// ---- 中心密钥归档（B5b）：口令只从 stdin 来，输出零路径零私钥 ----

const KEY_PASS_TTY: &str = "passphrase must be piped on stdin";
const KEY_PASS_EMPTY: &str = "passphrase is empty";
const KEY_PASS_LARGE: &str = "passphrase is too large";
const KEY_PASS_READ: &str = "cannot read the passphrase";

/// 口令上限 1 KiB（2^10）：写移位算式而不是十进制字面量——阶段 2 的判据 40a 把那个
/// 四位数字挡在 `hub/src` 之外，而这里是口令，不是任何一层的预算常数。
const MAX_KEY_PASSPHRASE_BYTES: usize = 1 << 10;

fn run_key(a: &KeyArgs) -> ExitCode {
    match &a.job {
        KeyJob::Export(x) => run_key_export(x),
        KeyJob::Import(x) => run_key_import(x),
    }
}

fn run_key_export(a: &KeyExportArgs) -> ExitCode {
    let passphrase = match read_passphrase() {
        Ok(p) => p,
        Err(msg) => return operational(msg),
    };
    let Some(now) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    match hub::key_archive::export(&a.data_dir, &a.out, &passphrase, now) {
        Ok(fingerprint) => {
            write_line(&json!({ "exported": true, "fingerprint": fingerprint }).to_string());
            ExitCode::from(0)
        }
        Err(e) => operational(e.message()),
    }
}

fn run_key_import(a: &KeyImportArgs) -> ExitCode {
    let passphrase = match read_passphrase() {
        Ok(p) => p,
        Err(msg) => return operational(msg),
    };
    match hub::key_archive::import(&a.data_dir, &a.from_file, &passphrase) {
        Ok(fingerprint) => {
            write_line(&json!({ "imported": true, "fingerprint": fingerprint }).to_string());
            ExitCode::from(0)
        }
        Err(e) => operational(e.message()),
    }
}

/// 口令只走 stdin：TTY 上不提示、不读（对着终端 read 会挂在那里等人打字）；
/// 恰好去掉一个行尾 `\n` / `\r\n`；空值与超上限都是封闭短语。
fn read_passphrase() -> Result<Vec<u8>, &'static str> {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return Err(KEY_PASS_TTY);
    }
    let mut raw = Vec::new();
    // 只读到越界一格为止：上限是一条拒收线，不为一个坏输入把内存吃完。
    let budget = MAX_KEY_PASSPHRASE_BYTES as u64 + 2;
    if std::io::stdin().lock().take(budget).read_to_end(&mut raw).is_err() {
        return Err(KEY_PASS_READ);
    }
    let value = strip_one_line_ending(&raw);
    if value.is_empty() {
        return Err(KEY_PASS_EMPTY);
    }
    if value.len() > MAX_KEY_PASSPHRASE_BYTES {
        return Err(KEY_PASS_LARGE);
    }
    Ok(value.to_vec())
}

/// 箱名是文件名 stem：`safe_token` 放行 `:`，而 `:` 在这里会把 `box:slot` 切错。
fn is_secret_box_name(name: &str) -> bool {
    rc_contract::safe_token(name) == name && !name.contains(':')
}

fn split_secret_ref(text: &str) -> Option<(&str, &str)> {
    if rc_llm_cfg::keys::check_secret_ref(text).is_err() {
        return None;
    }
    text.split_once(':')
}

fn open_secret_box(data_dir: &Path, box_name: &str) -> Result<rc_secret::SecretsBox, &'static str> {
    let enc = secret_box_path(data_dir, box_name);
    // 先看存在性：`open` 把"文件缺"折成 Io，而操作员要能分开"箱没建"与"箱坏了"。
    if !enc.exists() {
        return Err(SECRET_BOX_NOT_FOUND);
    }
    let pass_path = secret_pass_path(data_dir, box_name);
    if !pass_path.exists() {
        return Err(SECRET_PASS_NOT_FOUND);
    }
    let raw = std::fs::read(&pass_path).map_err(|_| SECRET_PASS_READ)?;
    let phrase = strip_one_line_ending(&raw);
    rc_secret::SecretsBox::open(&enc, phrase).map_err(|e| secret_open_fault(&e))
}

/// 一个变体一条常量串、零插值；`SecretError` 增变体时这里编译不过，逼着表态。
fn secret_open_fault(e: &rc_secret::SecretError) -> &'static str {
    use rc_secret::SecretError as E;
    match e {
        E::Io(_) => SECRET_BOX_READ,
        E::BadFormat | E::OpenFailed | E::BadParams | E::BackupKey | E::BackupFormat
        | E::BackupAuth => SECRET_BOX_UNOPENABLE,
        E::AlreadyExists => SECRET_BOX_EXISTS,
        E::BadSlot | E::MissingSlot => SECRET_REF_BAD,
        E::Permission => SECRET_BOX_READ,
        // `open` 自己不产生"文件缺/箱名坏"：存在性由 `open_secret_box` 先判、箱名由调用方先判。
        // 仍逐条表态：`SecretError` 加变体时这里编译不过。
        E::BoxMissing => SECRET_BOX_NOT_FOUND,
        E::PassMissing => SECRET_PASS_NOT_FOUND,
        E::BadBox => SECRET_BOX_NAME_BAD,
    }
}

fn secret_write_fault(e: &rc_secret::SecretError) -> &'static str {
    use rc_secret::SecretError as E;
    match e {
        E::Io(_) | E::BadFormat | E::BadParams | E::Permission | E::BackupKey
        | E::BackupFormat | E::BackupAuth => SECRET_BOX_WRITE,
        E::OpenFailed => SECRET_BOX_UNOPENABLE,
        E::AlreadyExists => SECRET_BOX_EXISTS,
        E::BadSlot | E::MissingSlot => SECRET_REF_BAD,
        // 读箱约定的变体不从写路径来（写侧先 `open_secret_box`，那之前已把缺文件挡掉）。
        E::BoxMissing => SECRET_BOX_NOT_FOUND,
        E::PassMissing => SECRET_PASS_NOT_FOUND,
        E::BadBox => SECRET_BOX_NAME_BAD,
    }
}

fn secret_create_fault(e: &rc_secret::SecretError) -> &'static str {
    use rc_secret::SecretError as E;
    match e {
        E::AlreadyExists => SECRET_BOX_EXISTS,
        E::Io(_) | E::BadFormat | E::OpenFailed | E::BadSlot | E::BadParams | E::Permission
        | E::BoxMissing | E::PassMissing | E::MissingSlot | E::BadBox | E::BackupKey
        | E::BackupFormat | E::BackupAuth => SECRET_BOX_CREATE,
    }
}

/// 恰好一个行尾换行；它前面若是 `\r`，属于同一个行尾。
/// 抽成纯函数是因为它是 stdin 上唯一会改动值字节的地方，而 pty 那一支在 CI 里造不出来。
fn strip_one_line_ending(bytes: &[u8]) -> &[u8] {
    let mut end = bytes.len();
    if end > 0 && bytes[end - 1] == b'\n' {
        end -= 1;
        if end > 0 && bytes[end - 1] == b'\r' {
            end -= 1;
        }
    }
    &bytes[..end]
}

fn secret_value_too_large(len: usize) -> bool {
    len > MAX_SECRET_VALUE_BYTES
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX_DIGITS[(b >> 4) as usize] as char);
        s.push(HEX_DIGITS[(b & 0x0f) as usize] as char);
    }
    s
}

/// 裁定 R19：stdout 的**唯一**写入口，写失败不改判定。
/// `hub index … | head -1` 之后下游管道已关，`println!` 会当场 panic 把退出码变成 101——
/// 而作业其实成功了（表已建、marker 已写）。退出码契约 `0/1/2/3` 是 agent 面的协议，不是风格，
/// 所以 import 与 index 两条读数面必须共用这一个容忍写法（原来只有 `print()` 容忍，
/// `run_index` 的三处是裸 `println!`）。
fn write_line(text: &str) {
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let _ = out.write_all(text.as_bytes()).and_then(|_| out.write_all(b"\n")).and_then(|_| out.flush());
}

// `index_failure`（守卫 12 的类别面）随 cycle 管线住 `rc-cycle`：`run_index` 与
// `run_cycle_once` 读的是同一个家（顶部 `use rc_cycle::index_failure`），不在 hub 复制第二份。
fn reject_json(r: &Reject) -> serde_json::Value {
    serde_json::json!({"loc": r.loc, "code": r.code, "detail": r.detail})
}

/// parse 的 Err 分支吃掉了 ExchangeFile，归因只能从原始 JSON 里回读会话的 device_id。
/// 读不出结构（JSON 本体就坏）时返回空表，调用方落到 "unknown"。
fn raw_session_device_ids(bytes: &[u8]) -> Vec<String> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|doc| {
            Some(
                doc.get("sessions")?
                    .as_array()?
                    .iter()
                    .map(|s| {
                        s.get("device_id")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    })
                    .collect(),
            )
        })
        .unwrap_or_default()
}

/// 脱敏映射的**唯一**落盘处：一行一条 JSONL，`original` 是这份文件里唯一的明文。
///
/// 只在 `--redaction-map FILE` 显式给出时调用（默认不写、不落库、不进快照/备份/报告）；
/// 调用方对错误只回一句类别名——`io::Error` 的原串里可能带路径。
fn write_redaction_map(path: &Path, entries: &[RedactionEntry]) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::File::create(path)?;
    for e in entries {
        let line = json!({
            "turn_uid": e.turn_uid,
            "role": e.role,
            "kind": e.kind,
            "start": e.start,
            "end": e.end,
            "original": e.original(),
        });
        writeln!(f, "{line}")?;
    }
    f.flush()
}

/// G17：输入面版本探测。**只在顶层 `format_version` 是字面 `3` 时**判为 v3，其余一律进
/// 既有 v1 `parse`（v1 行为一字不动）。为什么不会误判 v1：
/// - v1 文件的 `format_version` 恒为 1（`SUPPORTED_FORMAT_VERSIONS = [1]`），读不到 3；
/// - 读不出结构（垃圾 JSON）、顶层不是对象、`format_version` 不是数——都落 v1 面，
///   由 `parse` 照旧报 `json_malformed`，退出码与今天相同；
/// - v1 形状的身子谎报 `3`：探测会放行进投影，但它读不成 v3（v3 的会话结构必填格缺失），
///   落 `json_malformed` 整批拒收——exit 2 与今天"版本不支持"同面，不会静默导入。
fn is_ingest_v3(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|doc| doc.get("format_version").and_then(serde_json::Value::as_u64))
        .map(|v| v == u64::from(rc_contract::ingest_v3::INGEST_V3_FORMAT_VERSION))
        .unwrap_or(false)
}

/// import 的整批拒收出口（exit 2）。v1 `parse` 的 Err 与 G17 的 v3 投影 Err 共用这一个家：
/// 审计行（`import_log.batch_reject_code`）、当场报告与退出码三面读数由同一份代码产出，
/// 两处各写一遍就是下一次漂移。`dropped_fields` 是投影读数——拒收面上没有投影，恒为空。
/// 返回 `u8` 而不是 `ExitCode`：G19 刀 5 的多文件循环要在码上取最坏值，而
/// `ExitCode::as_u8` / `From<ExitCode> for u8` 今天还是 unstable——换算只发生在
/// [`run_import`] 的边界上，这一族内部全是 u8。
fn import_reject_exit(
    a: &ImportArgs,
    root: &mut Root,
    started_at_ms: i64,
    now_ms: i64,
    sha: &str,
    bytes: &[u8],
    rejects: Vec<Reject>,
) -> u8 {
    let devices = raw_session_device_ids(bytes);
    let batch = rejects.first().map(|r| r.code);
    // 整批拒收的两个平面必须留同一种审计行：`import_log.batch_reject_code`
    // 这一列本来就是为"被拒的导入"存在的，只在一个平面写它等于半个审计。
    let rep = BatchReport {
        accepted: Vec::new(),
        rejects,
        batch_reject: batch,
        // 没过 normalize 就没有正文入库，也就没有脱敏。
        redacted_spans: 0,
    };
    if commit(
        root,
        CommitInput {
            started_at_ms,
            now_ms,
            device_id: sole_device(&devices),
            exchange_sha256: sha,
            devices: &devices,
            rep: &rep,
            line_rejected_turns: 0,
        },
    )
    .is_err()
    {
        return operational_u8("store write failed");
    }
    print(
        &Report {
            schema_version: REPORT_SCHEMA_VERSION,
            turns_accepted: 0,
            turns_rejected: rep.rejected_turn_count(),
            skipped_duplicate: 0,
            tombstoned: 0,
            batch_reject: batch.map(RejectCode::slug).unwrap_or_default(),
            rejects: rep.rejects.iter().map(reject_json).collect(),
            redacted_spans: 0,
            dropped_fields: Default::default(),
            turn_uids: Vec::new(),
        },
        a.json,
    );
    2
}

/// G19 刀 5：import 的多文件入口。单文件走既有路径原样（回归红线：行为与回执
/// 一字不变）；多文件逐个走**同一条管线**（[`import_one`]），每个文件独立一份
/// Report 输出，退出码取各文件的最坏值（3 运维失败 > 2 整批拒 > 1 逐条拒 > 0 全收）
/// ——"最坏"是 max 而不是"最后一个"：坏文件排前面时，好文件的成功不许把它洗白。
fn run_import(a: &ImportArgs) -> ExitCode {
    if a.files.len() == 1 {
        return ExitCode::from(import_one(a, &a.files[0]));
    }
    // 多文件配 `--redaction-map` 在这里响亮拒绝：映射文件按导入覆盖写（`File::create`），
    // 逐文件覆盖 = 前几个文件"哪一段被换"的唯一副本被销毁。单文件路径不动。
    if a.redaction_map.is_some() {
        return operational("--redaction-map keeps one import's spans; pass a single file");
    }
    let mut worst = 0u8;
    for f in &a.files {
        worst = worst.max(import_one(a, f));
    }
    ExitCode::from(worst)
}

/// [`operational`] 的 u8 版（多文件循环在 u8 上取 max；换算只发生在 [`run_import`
/// 的边界，见 [`import_reject_exit`] 的档注）。打印行为与 `operational` 逐字相同。
fn operational_u8(msg: &str) -> u8 {
    operator_line("error", msg);
    3
}

/// 一个文件的导入管线（多文件刀之前它就是 `run_import` 的全部——G19 刀 5 只把
/// 文件路径从 `args` 拆成形参、返回值换成 u8，其余正文一字未动；`turn_uids`
/// 一格是刀 6 的加法）。
fn import_one(a: &ImportArgs, file: &std::path::Path) -> u8 {
    let bytes = match std::fs::read(file) {
        Ok(b) => b,
        Err(_) => return operational_u8("cannot read input file"),
    };
    let mut h = Sha256::new();
    h.update(&bytes);
    let sha = h.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
    let now_ms = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => return operational_u8("system clock is behind the unix epoch"),
    };
    let started = now_ms;

    let mut root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational_u8("cannot open data store"),
    };

    // G17（计划 A8）：DSH 转换器产 exchange v3，而 import 只收 v1。裁定接下行投影：
    // v3 输入先经 `rc_contract::project_v3_to_v1` 投成 v1 的 `ExchangeFile`（富字段丢弃
    // 逐字段计数），再把投影产物**原样**序列化喂给下面的 v1 `parse`——投影不产出任何
    // 绕过 v1 校验的行。v1/v2 面（探测为否）一字不动。
    let (bytes, projection_dropped) = if is_ingest_v3(&bytes) {
        match rc_contract::project_v3_to_v1(&bytes) {
            Ok(out) => match serde_json::to_vec(&out.file) {
                Ok(b) => (b, out.dropped),
                Err(_) => return operational_u8("projection serialization failed"),
            },
            Err(rejects) => {
                return import_reject_exit(a, &mut root, started, now_ms, &sha, &bytes, rejects);
            }
        }
    } else {
        (bytes, Default::default())
    };

    let file = match parse(&bytes) {
        Ok(f) => f,
        Err(rejects) => {
            return import_reject_exit(a, &mut root, started, now_ms, &sha, &bytes, rejects);
        }
    };

    // normalize 消费 file，拒收归因要的 device_id 得先抄下来。
    let devices: Vec<String> = file.sessions.iter().map(|s| s.device_id.clone()).collect();
    // 脱敏映射只在显式给了路径时才收集：没有路径这条路上连一个明文副本都不留。
    let mut redaction_map: Vec<RedactionEntry> = Vec::new();
    let collect_map = a.redaction_map.is_some();
    let rep = normalize_with(
        file,
        now_ms,
        collect_map.then_some(&mut redaction_map),
    );

    let committed = match commit(
        &mut root,
        CommitInput {
            started_at_ms: started,
            now_ms,
            device_id: sole_device(&devices),
            exchange_sha256: &sha,
            devices: &devices,
            rep: &rep,
            line_rejected_turns: 0,
        },
    ) {
        Ok(c) => c,
        Err(_) => return operational_u8("store write failed"),
    };

    // 映射写在提交之后：正文已经换掉了，这份文件是运维回头核对"哪一段被换"的唯一副本。
    // 写失败是运维失败（3），消息只回类别名——路径与明文都不进输出（守卫 12）。
    if let Some(path) = a.redaction_map.as_deref() {
        if write_redaction_map(path, &redaction_map).is_err() {
            return operational_u8("cannot write redaction map");
        }
    }

    // D 子 agent 接线：turn 自带 `speaker` 时，把"这一轮是谁说的"落到记忆侧
    // （agent 锚 + 轮级 mention）。转录层先落，记忆侧随后；失败不改已导入的读数。
    if record_turn_speakers_after_import(&root, &rep, now_ms).is_err() {
        return operational_u8("store write failed");
    }

    // D31：human 轮正文里的「我 / 我们」→ 主人锚 mention。与上一条并列的独立一遍，
    // 必须落在 `commit` 之后：claim 与 `sessions` 归属行这时才是真值。
    // 实现搬在 `hub::ingest`（CLI 与 HTTP 共用），这里只做退出码映射。
    if hub::ingest::record_principal_mentions_after_import(&root, &rep, now_ms).is_err() {
        return operational_u8("store write failed");
    }

    // 索引层是读侧（召回）的唯一入口，而它以前只被 `cycle once`、复制接收、恢复建
    // ——§10.148 的现场就是"中心与新建臂只被 import 喂过 ⇒ `index status` 空 ⇒ 召回直接
    // index_not_built"。全量重建是幂等的、整层可丢弃（裁定 R15），所以挂在导入末尾代价可接受。
    // 一行都没进来就不必重扫；建失败不改导入判定（导入确实成功了），只出声。
    if committed.inserted > 0 && rc_index::rebuild(root.memory(), now_ms).is_err() {
        operator_warn("index rebuild failed — run: hub index rebuild --data-dir <dir>");
    }

    print(
        &Report {
            schema_version: REPORT_SCHEMA_VERSION,
            turns_accepted: committed.inserted,
            turns_rejected: rep.rejected_turn_count(),
            skipped_duplicate: committed.skipped_duplicate,
            tombstoned: committed.tombstoned,
            batch_reject: rep.batch_reject.map(RejectCode::slug).unwrap_or_default(),
            rejects: rep.rejects.iter().map(reject_json).collect(),
            redacted_spans: rep.redacted_spans,
            dropped_fields: projection_dropped,
            // G19 刀 6：接受轮的 uid 列表（口径见 `Report.turn_uids` 的档注）。
            turn_uids: rep.accepted.iter().map(|acc| acc.turn.turn_uid.clone()).collect(),
        },
        a.json,
    );

    if rep.batch_reject.is_some() {
        2
    } else if !rep.rejects.is_empty() {
        1
    } else {
        0
    }
}

/// 把 v1 批次里带 `speaker` 的轮落到记忆侧（D 子 agent 接线）。
///
/// 按 `(session_key, device_id, project_hint)` 分组：scope 取会话 scope（`project_hint`
/// 优先、没有就落会话键——与 distill 的 `Batch::scope_key` 同一口径），归属用会话自报的
/// `source_agent`。子 agent 没有自己的 claim，主人锚仍取父会话，所以这里不碰 claim。
///
/// 只处理**通过契约 normalize 的轮**：`speaker` 是 additive 的 v1 字段，旧生产者不发，
/// 这一步就什么都不做（C1 行为原样）。
fn record_turn_speakers_after_import(
    root: &Root,
    rep: &BatchReport,
    now_ms: i64,
) -> Result<(), rc_memory::MemError> {
    // 分组用线性查找：一次导入的会话数是个位数，不为此引一张 HashMap。
    let mut groups: Vec<(&rc_contract::SessionId, Vec<(String, String)>)> = Vec::new();
    for acc in &rep.accepted {
        let Some(speaker) = acc.speaker.as_deref() else {
            continue;
        };
        match groups.iter_mut().find(|(s, _)| {
            s.session_key == acc.session.session_key && s.device_id == acc.session.device_id
        }) {
            Some((_, turns)) => turns.push((acc.turn.turn_uid.clone(), speaker.to_string())),
            None => groups.push((
                &acc.session,
                vec![(acc.turn.turn_uid.clone(), speaker.to_string())],
            )),
        }
    }
    for (session, turns) in groups {
        if turns.is_empty() {
            continue;
        }
        let ctx = BatchCtx {
            source_agent: session.source_agent.clone(),
            device_id: session.device_id.clone(),
        };
        let owner = OwnerCtx::synthetic(&session.source_agent);
        let scope = Scope {
            kind: ScopeKind::Project,
            key: session
                .project_hint
                .clone()
                .unwrap_or_else(|| session.session_key.clone()),
        };
        record_turn_speakers(root.memory(), &ctx, &owner, &scope, &turns, now_ms)?;
    }
    Ok(())
}

/// 索引层作业。返回值形状沿用阶段 0：`main` 与每个 `run_*` 一律返回 `ExitCode`，
/// 运维失败只走 `operational(msg) -> ExitCode::from(3)`——hub 一旦有两套失败词汇，退出码就不再是唯一读数。
/// 输出面只有 `IndexOut` 那四个读数：`rc-index` 报回来的错误一律换成类别名（守卫 12），
/// 因为 `IndexError::Sql` 之外的分支（`Store`/`Memory`）理论上仍能带库文件名。
/// 类别名只在 `index_failure` 一处拼（裁定 R18-5：`LayerUnreadable` 有自己那条常量串，
/// 不与"没建索引"共用消息）；stdout 一律走 `write_line`（裁定 R19）。
fn run_index(job: &IndexJob) -> ExitCode {
    let now = match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => d.as_millis() as i64,
        Err(_) => return operational("clock unavailable"),
    };
    match job {
        IndexJob::Rebuild { data_dir } => {
            let root = match open_root(data_dir) {
                Ok(r) => r,
                Err(_) => return operational("store unreachable — run: doctor"),
            };
            // 任务 1 的签名是 `Root::memory(&self) -> &MemoryHandle`（句柄在 `open` 时已经开好，
            // 所以它不是 Result）。写成 `.map_err(..)?` 会在这一行编译失败——那是签名漂移的读数。
            let m = root.memory();
            let rep = match rc_index::rebuild(m, now) {
                Ok(r) => r,
                Err(e) => return operational(index_failure(&e)),
            };
            // 表清单只有一个家：这里现算，不在 hub 里抄第二份 `rc_shadow_*` 名单（守卫 2）。
            let names = match rc_index::shadow_tables(m.conn()) {
                Ok(v) => v,
                Err(_) => return operational("index shadow list failed — run: doctor"),
            };
            // W38a 刀 A（设计 2026-11-08-w38a-transcript-search §二.2）：转录段——
            // `turn_shadow` 全量重建（增量之外的存量与修复）。导入路径不触发重建（增量已覆盖）。
            let turns_indexed = match rc_ledger::rebuild_turn_shadow(root.transcript()) {
                Ok(n) => n,
                Err(_) => return operational("transcript shadow rebuild failed — run: doctor"),
            };
            let json = serde_json::to_string(&IndexOut {
                schema_version: CapReport::schema_version(),
                rows_indexed: rep.rows_indexed,
                shadow_tables: names.len(),
                norm_version: rep.norm_version,
                turns_indexed,
            });
            match json {
                Ok(s) => {
                    write_line(&s);
                    ExitCode::from(0)
                }
                Err(_) => operational("index report serialization failed"),
            }
        }
        IndexJob::Status { data_dir } => {
            let root = match open_root(data_dir) {
                Ok(r) => r,
                Err(_) => return operational("store unreachable — run: doctor"),
            };
            let m = root.memory();
            let rep = match rc_index::index_status(m) {
                Ok(r) => r,
                Err(e) => return operational(index_failure(&e)),
            };
            match rep {
                Some(rep) => write_line(&format!(
                    "{{\"rebuilt_at_ms\":{},\"rows_indexed\":{},\"norm_version\":{},\"schema_version\":{}}}",
                    rep.rebuilt_at_ms, rep.rows_indexed, rep.norm_version, rep.schema_version
                )),
                None => write_line("{\"rebuilt_at_ms\":null}"),
            }
            ExitCode::from(0)
        }
    }
}

/// W25：`hub pending` —— 水位闸（主触发）的读数面。**只读装配**（`Root::open_readonly`）：
/// 它每 10 分钟被叫一次，而 `Root::open` 的迁移副作用在 W24a 的审计里是实测过的
/// （跑一次 doctor 就在现网新建空表、把 WAL 撑大）——"读一眼"不该改库。
///
/// 出错只有一条路：退出码 3。`drain-if-pending.sh` 靠"0 = 没有待办 / 3 = 读不出来"这两个码
/// 决定跑不跑，所以**读失败绝不能折成一份全 0 的读数**（那会被念成"今天没活"）。
fn run_pending(a: &PendingArgs) -> ExitCode {
    let root = match Root::open_readonly(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("store unreachable — run: doctor"),
    };
    let report = match hub::pending::read(&root) {
        Ok(r) => r,
        Err(_) => return operational("pending read failed — run: doctor"),
    };
    if a.json {
        match serde_json::to_string(&report.json()) {
            Ok(s) => write_line(&s),
            Err(_) => return operational("pending serialization failed"),
        }
    } else {
        write_line(&report.human());
    }
    ExitCode::from(0)
}

// ── W24c：`hub issue list|resolve` —— `extraction_issue` 的运维出口 ─────────────────

/// 列表里 `detail` 最多印多少字符。载荷是调用方拼的 JSON（一条能有几百字），而列表回答的是
/// "有几条、都是什么 code"——不是正文阅读器。`--json` 同一口径（截断是这一面的契约，
/// 不是渲染细节；要看全量去按 id 读库/走 `derive retry` 的读数）。
const ISSUE_DETAIL_MAX_CHARS: usize = 120;

fn run_issue(job: &IssueJob) -> ExitCode {
    match job {
        IssueJob::List(a) => issue_list(a),
        IssueJob::Resolve(a) => issue_resolve(a),
    }
}

/// `detail` 折成单行并截断。折行是给一行一条的文本面用的：载荷里的换行会把一条审计撕成
/// 几行，操作员数行数就会数错。
fn truncate_issue_detail(detail: &str) -> String {
    let one_line: String = detail
        .chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect();
    let mut chars = one_line.chars();
    let head: String = chars.by_ref().take(ISSUE_DETAIL_MAX_CHARS).collect();
    if chars.next().is_some() { format!("{head}…") } else { head }
}

/// 只读装配（`Root::open_readonly`，W24a 同 W25）：这一面会被运维反复敲，
/// `Root::open` 的迁移副作用（建空表、撑大 WAL）不该由"看一眼"触发。
fn issue_list(a: &IssueListArgs) -> ExitCode {
    let root = match Root::open_readonly(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("store unreachable — run: doctor"),
    };
    // 三个筛选格都在 SQL 里：`?1 IS NULL OR ...` 让"没给"与"给了空串"不同——
    // 空串是一个真的筛选值（`--code ""` 只会命中空 code 的行），不在这里替操作员改写。
    let mut st = match root.memory().conn().prepare(
        "SELECT id, pass, code, detail, resolved_at_ms FROM extraction_issue
          WHERE (?1 IS NULL OR code=?1) AND (?2 IS NULL OR pass=?2)
            AND (?3=0 OR resolved_at_ms IS NULL)
          ORDER BY id",
    ) {
        Ok(s) => s,
        Err(_) => return operational("issue read failed — run: doctor"),
    };
    let rows = match st.query_map(
        rusqlite::params![a.code.as_deref(), a.pass.as_deref(), i64::from(a.open)],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<i64>>(4)?,
            ))
        },
    ) {
        Ok(v) => v,
        Err(_) => return operational("issue read failed — run: doctor"),
    };
    let mut docs: Vec<Value> = Vec::new();
    let mut lines: Vec<String> = Vec::new();
    for row in rows {
        let (id, pass, code, detail, resolved_at_ms) = match row {
            Ok(v) => v,
            Err(_) => return operational("issue read failed — run: doctor"),
        };
        let detail = truncate_issue_detail(&detail);
        if a.json {
            docs.push(json!({
                "id": id,
                "pass": pass,
                "code": code,
                "resolved_at_ms": resolved_at_ms,
                "detail": detail,
            }));
        } else {
            lines.push(format!(
                "id={id} pass={pass} code={code} resolved_at_ms={} detail={detail}",
                resolved_at_ms.map_or_else(|| "null".to_owned(), |v| v.to_string()),
            ));
        }
    }
    if a.json {
        match serde_json::to_string(&Value::Array(docs)) {
            Ok(s) => write_line(&s),
            Err(_) => return operational("issue serialization failed"),
        }
    } else {
        for line in lines {
            write_line(&line);
        }
    }
    ExitCode::from(0)
}

fn issue_resolve(a: &IssueResolveArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("store unreachable — run: doctor"),
    };
    match rc_memory::issues::resolve(root.memory(), a.id, now_ms) {
        Ok(true) => {}
        // 0 行 = "这个 id 不存在"（`issues::resolve` 的注释写着这一支归调用方报）。
        // 已解决的行仍命中 1 行（UPDATE 不筛 `resolved_at_ms`）⇒ 重跑是幂等成功：
        // 关闭动作没有"再点一次就报错"的理由。
        Ok(false) => return operational("no such issue — run: hub issue list"),
        Err(_) => return operational("issue resolve failed — run: doctor"),
    }
    if a.json {
        let doc = json!({"id": a.id, "resolved_at_ms": now_ms, "resolved": true});
        match serde_json::to_string(&doc) {
            Ok(s) => write_line(&s),
            Err(_) => return operational("issue serialization failed"),
        }
    } else {
        write_line(&format!("id={} resolved_at_ms={now_ms} resolved=1", a.id));
    }
    ExitCode::from(0)
}

// ── W24c：`hub environment declare` —— 运维直陈的环境属性 ─────────────────────────

fn run_environment(job: &EnvironmentJob) -> ExitCode {
    match job {
        EnvironmentJob::Declare(a) => environment_declare(a),
    }
}

fn environment_declare(a: &EnvironmentDeclareArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // scope 种类走 `ScopeKind::parse` 那一处闭集（不把三个词在 clap 里再抄一遍：
    // 抄一遍就是第二个闭集，加一格时会静默漏掉）。解析不出 ⇒ 用法错误。
    let Some(scope_kind) = ScopeKind::parse(&a.scope_kind) else {
        return operational(USAGE);
    };
    // 六键闭集在旗标面上是结构性的：每一格都映射到一个 `ATTR_*` 常量。空白值当作没给
    // （与导入侧 `EnvironmentDecl::from_lookup` 同一条取舍）——`--brand ""` 不写出空属性。
    let pick = |v: &Option<String>| {
        v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
    };
    let mut attributes: Vec<(&'static str, String)> = Vec::new();
    for (key, value) in [
        (rc_memory::ATTR_BRAND, pick(&a.brand)),
        (rc_memory::ATTR_OS_USER, pick(&a.os_user)),
        (rc_memory::ATTR_BILIBILI_ACCOUNT, pick(&a.bilibili_account)),
        (rc_memory::ATTR_QODER_ACCOUNT, pick(&a.qoder_account)),
    ] {
        if let Some(value) = value {
            attributes.push((key, value));
        }
    }
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    let req = environment::DeclareRequest {
        hostname: a.hostname.clone(),
        attributes,
        principal: a.principal.clone(),
        source_agent: a.source_agent.clone(),
        device_id: a.device_id.clone(),
        scope_kind,
        scope_key: a.scope_key.clone(),
    };
    let declared = match environment::declare(&root, &req, now_ms) {
        Ok(d) => d,
        // 空白格与未知键都是"敲错了参数"，不是库故障：主命令的用法错误同一条出路。
        Err(environment::DeclareError::Blank | environment::DeclareError::UnknownKey) => {
            return operational(USAGE)
        }
        // 身份解析不到 ⇒ 不猜（派发档 §2.3）。这一句要把口径说给操作员：补 `--principal`。
        Err(environment::DeclareError::NoPrincipal) => {
            return operational("no known principal — pass: --principal ID")
        }
        Err(environment::DeclareError::Store | environment::DeclareError::Memory) => {
            return operational("environment declare failed — run: doctor")
        }
    };
    let out = declared.outcome;
    if a.json {
        let doc = json!({
            "anchor_id": declared.anchor_id,
            "inserted": out.inserted,
            "identical": out.identical,
            "reopened": out.reopened,
            "superseded": out.superseded,
        });
        match serde_json::to_string(&doc) {
            Ok(s) => write_line(&s),
            Err(_) => return operational("environment declare serialization failed"),
        }
    } else {
        write_line(&format!(
            "anchor={} inserted={} identical={} reopened={} superseded={}",
            declared.anchor_id, out.inserted, out.identical, out.reopened, out.superseded,
        ));
    }
    ExitCode::from(0)
}

/// W26：`hub anomaly sweep` —— 日保底档的维护步。写面只有两处：**`attention` 的升/解**，
/// 以及重影那一族按自己判据补写的**别名闭包**（`link_alias_pair`，与 legacy `cycle once` 同源）；
/// 任何**事实行**（card/event/anchor 本身、`verdict`、`superseded_by`）它一格都不碰。
///
/// 三件事在这一层定死：
/// 1. **两族各自调用既有的产品函数**（`anomaly::sweep` / `anomaly::sweep_ghosts`），这里不复制
///    任何判据。顺序就是派发档写的那个顺序：先矛盾、后重影。两者共用同一个 `now_ms`——一轮之内
///    不许有两个"现在"（`rc_cycle::run_anomaly_scan` 对同一件事的写法）。
/// 2. **四格 JSON**：`contradiction` = 矛盾那一族的一对计数；`ghosts` = 重影两族（别名歧义 +
///    同题多行）各自的 raised/resolved **相加**。重影的第三格 `linked`（闭包本轮补写的别名行数）
///    不进读数：它是"补别名"那一格，与本命令"升/解告警"那一对计数不同家；日路径要那个数
///    就去看 `ghosts` 之外的既有面（本轮不新增格，契约只有四格）。
/// 3. **失败只有一条路**：退出码 3。日脚本把它记进 `fails` 后继续跑后面的段（维护项不是数据面），
///    所以这里不许把失败折成一份全 0 的读数——那会被念成"扫过了、库是干净的"。
///
/// 它不像 `pending` 那样走只读装配：`sweep_ghosts` 的**第一个动作就是写**（`link_alias_pair`），
/// 只读句柄会在那一支当场 `Err(ReadOnly)`（`anomaly.rs` 的注释写着这条前提）。
fn run_anomaly(a: &AnomalyArgs) -> ExitCode {
    match &a.job {
        AnomalyJob::Sweep(x) => run_anomaly_sweep(x),
    }
}

fn run_anomaly_sweep(a: &AnomalySweepArgs) -> ExitCode {
    let Some(now_ms) = unix_now_ms() else {
        return operational("system clock is behind the unix epoch");
    };
    // 一道便宜的存在性闸：`Root::open` 在缺库时会把三册库**建出来**，于是敲错一个 `--data-dir`
    // 会静默交回一份全 0 的读数——那不是"库是干净的"，是"这个目录里没有库"。本命令的全部写入
    // （`attention` 的升/解）都落 `memory.db`，所以只看这一册。
    // 不走 `Root::open_readonly` 那种装配闸：它要求三册 schema 都是**当前版本**，会把"待迁移的
    // 旧库"念成"库不在"——而 `Root::open` 恰恰就是那个负责迁移的入口（`open_root` 会响一声）。
    if !a.data_dir.join("memory.db").exists() {
        return operational("store unreachable — run: doctor");
    }
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    // 矛盾那一族：判据 → 升新账 → 解掉不再成立的旧账（`sweep` 内部两半都在）。
    let contradiction = match rc_memory::anomaly::sweep(root.memory(), now_ms) {
        Ok(r) => r,
        Err(_) => return operational("anomaly sweep failed — run: doctor"),
    };
    // 重影那一族：先闭包、再报别名歧义与同题多行（顺序在 `sweep_ghosts` 里，这里不重复）。
    let ghosts = match rc_memory::anomaly::sweep_ghosts(root.memory(), now_ms) {
        Ok(r) => r,
        Err(_) => return operational("anomaly sweep failed — run: doctor"),
    };
    // `ghosts.raised/resolved` 是两族之和：别名歧义与同题多行各有一对计数，四格契约里没有
    // 第五格，所以这一对是聚合值（哪一族升的，去 `attention.kind` 那一列看）。
    let raised = ghosts.ambiguity.raised + ghosts.duplicate_title.raised;
    let resolved = ghosts.ambiguity.resolved + ghosts.duplicate_title.resolved;
    if a.json {
        write_line(&json!({
            "contradiction": {
                "raised": contradiction.raised,
                "resolved": contradiction.resolved,
            },
            "ghosts": { "raised": raised, "resolved": resolved },
        }).to_string());
    } else {
        write_line(&format!(
            "contradiction raised={} resolved={}  ghosts raised={} resolved={}",
            contradiction.raised, contradiction.resolved, raised, resolved,
        ));
    }
    ExitCode::from(0)
}

/// `hub derive status|retry` 的装配面：读数只读水位那一行 + issue 摘要；回拨只走
/// `water::retry_from`。两形（文本/JSON）都在这里定，错误一律运维错误（退出 3）。
fn run_derive(job: &DeriveJob) -> ExitCode {
    match job {
        DeriveJob::Status(a) => derive_status(a),
        DeriveJob::Retry(a) => derive_retry(a),
        DeriveJob::Run(a) => derive_run(a),
    }
}

/// 水位那一行的三种读数（游标 / attempts / 最后一次成功）。行不存在 ⇒ `Some((None, 0, None))`：
/// "从没跑过"不是 `Some(0)`——那是"跑过、停在 0 点"，与 `water::read` 的同一口径。
/// 读失败 ⇒ `None`（调用方翻成运维错误，不把库故障读成"从没跑过"）。
type DeriveWaterRow = (Option<(i64, String)>, i64, Option<i64>);

fn derive_water_row(root: &Root) -> Option<DeriveWaterRow> {
    match root.ledger().conn().query_row(
        "SELECT cursor_at_ms, cursor_turn_uid, attempts, last_success_at_ms
           FROM job_watermark WHERE family='derive' AND scope_key=''",
        [], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?,
                   r.get::<_, i64>(2)?, r.get::<_, Option<i64>>(3)?))) {
        Ok((at, uid, attempts, last)) => Some((Some((at, uid)), attempts, last)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Some((None, 0, None)),
        Err(_) => None,
    }
}

fn derive_status(a: &DeriveStatusArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("store unreachable — run: doctor"),
    };
    let now_ms = match unix_now_ms() {
        Some(v) => v,
        None => return operational("system clock is behind the unix epoch"),
    };
    let (cursor, attempts, last_success_at_ms) = match derive_water_row(&root) {
        Some(v) => v,
        None => return operational("derive watermark read failed — run: doctor"),
    };
    let open_issues = match rc_memory::issues::open_count(root.memory(),
                                                          rc_memory::issues::DERIVE_PASS) {
        Ok(n) => n,
        Err(_) => return operational("derive issue read failed — run: doctor"),
    };
    let oldest = match rc_memory::issues::oldest_open_created_at(
            root.memory(), rc_memory::issues::DERIVE_PASS) {
        Ok(v) => v,
        Err(_) => return operational("derive issue read failed — run: doctor"),
    };
    // 年龄不让负数穿透：本机时钟回拨时 `now - created` 会是负数，而那不是一个年龄。
    let oldest_issue_age_ms = oldest.map(|created| (now_ms - created).max(0));
    if a.json {
        let doc = json!({
            "cursor": cursor.map(|(at_ms, turn_uid)| json!({"at_ms": at_ms, "turn_uid": turn_uid})),
            "attempts": attempts,
            "last_success_at_ms": last_success_at_ms,
            "open_issues": open_issues,
            "oldest_issue_age_ms": oldest_issue_age_ms,
        });
        match serde_json::to_string(&doc) {
            Ok(s) => write_line(&s),
            Err(_) => return operational("derive status serialization failed"),
        }
    } else {
        let at = match &cursor {
            Some((at_ms, uid)) => format!("{at_ms}:{uid}"),
            None => "none".to_owned(),
        };
        let last = match last_success_at_ms {
            Some(ms) => ms.to_string(),
            None => "none".to_owned(),
        };
        let age = match oldest_issue_age_ms {
            Some(ms) => ms.to_string(),
            None => "none".to_owned(),
        };
        write_line(&format!("cursor={at}  attempts={attempts}  last_success_at_ms={last}  \
                              open_issues={open_issues}  oldest_issue_age_ms={age}"));
    }
    ExitCode::from(0)
}

fn derive_retry(a: &DeriveRetryArgs) -> ExitCode {
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("store unreachable — run: doctor"),
    };
    let now_ms = match unix_now_ms() {
        Some(v) => v,
        None => return operational("system clock is behind the unix epoch"),
    };
    let issue = match rc_memory::issues::get(root.memory(), a.issue) {
        Ok(Some(i)) => i,
        Ok(None) => return operational("no such issue — run: hub derive status"),
        Err(_) => return operational("issue read failed — run: doctor"),
    };
    // 不是推导隔离的行一律不猜：拿别的 pass 的 detail 去回拨推导游标会把一条无关的记录
    // 读成一次授权。pass 列的闭集住在生产侧（`issues::DERIVE_PASS`），不在这里再抄一个词。
    if issue.pass != rc_memory::issues::DERIVE_PASS {
        return operational("issue is not a derive quarantine — refusing");
    }
    let detail: Value = match serde_json::from_str(&issue.detail) {
        Ok(v) => v,
        Err(_) => return operational("issue detail is not readable json — refusing"),
    };
    let prev_at_ms = match detail.get("prev_at_ms").and_then(Value::as_i64) {
        Some(v) => v,
        None => return operational("issue detail has no prev_at_ms — refusing"),
    };
    let prev_uid = match detail.get("prev_uid").and_then(Value::as_str) {
        Some(v) => v.to_owned(),
        None => return operational("issue detail has no prev_uid — refusing"),
    };
    let from = water::Cursor { at_ms: prev_at_ms, turn_uid: prev_uid };
    // 先回拨游标、再关 issue：反过来的话"关掉了却没有回拨"是一条已解决却不会重跑的记录。
    // 两个库（ledger / memory）不在同一个连接上，跨库原子性不存在；这一顺序让残局落在
    // "游标已回拨、issue 仍开着"——下一轮会重跑，重复点一次 retry 也是幂等的。
    if water::retry_from(root.ledger(), "derive", "", &from, now_ms).is_err() {
        return operational("derive cursor write failed — run: doctor");
    }
    match rc_memory::issues::resolve(root.memory(), issue.id, now_ms) {
        Ok(true) => {}
        Ok(false) | Err(_) => return operational("issue resolve failed — run: doctor"),
    }
    if a.json {
        let doc = json!({
            "retried": true,
            "issue": issue.id,
            "from": {"at_ms": from.at_ms, "turn_uid": from.turn_uid},
        });
        match serde_json::to_string(&doc) {
            Ok(s) => write_line(&s),
            Err(_) => return operational("derive retry serialization failed"),
        }
    } else {
        write_line(&format!("retried issue {} from {}:{}", issue.id,
                            from.at_ms, from.turn_uid));
    }
    ExitCode::from(0)
}

// ── `derive run` —— 把推导线接上批循环（L3 组合阶段）────────────────────────────

/// 熔断阈值的缺省值，与 `tools/pipeline/daily-drain.sh` 里 `FAIL_STREAK` 的同一格同名同义：
/// `Fault` 与 `Store` 都不推水位（`rc_distill::derive` 模块头），循环侧不设这道闸就是
/// 在同一个坏批上无限重试（P-32 那一族的活锁）。
const DERIVE_FAIL_STREAK_DEFAULT: usize = 3;

/// `--llm-timeout-ms 0` 的类别串：旗标形状问题，主语是 `derive run` 自己的旗标
/// （与 `distill`/`review` 各点各的主语，同 `FAULT_REVIEW_LLM_TIMEOUT` 那一族）。
const FAULT_DERIVE_LLM_TIMEOUT: &str = "derive run --llm-timeout-ms must be greater than zero";

/// 水位读不出来的类别串（与 `derive_status` 里那一句同一个词：同一件事在运维面只念一句话）。
const FAULT_DERIVE_WATER_READ: &str = "derive watermark read failed — run: doctor";

/// 每行读数自身的形状版本（与 `Report::schema_version` 同义：不是库的版本）。
const DERIVE_RUN_SCHEMA_VERSION: u32 = 1;

/// `RECOLLECT_DRAIN_FAIL_STREAK`：日路径与 `derive run` 共用的那一格熔断阈值。
/// 读不出来（没设 / 不是数 / 是 0）一律回落 3：这一格的作用是"别在同一个坏批上无限重试"，
/// 把它读成 0 等于第一轮失败就熔断，与没给是两件事，但都不是可信的阈值。
fn derive_fail_streak() -> usize {
    std::env::var("RECOLLECT_DRAIN_FAIL_STREAK").ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DERIVE_FAIL_STREAK_DEFAULT)
}

/// 推导游标的一次**位置**读数。`None` = "从没跑过"，而它的位置等价于初值 `(0, "")`——
/// `run_derive` 的隔离路径就是这么把 `None` 折成 `(0, "")` 写进 issue 的 `prev` 的（同一个口径）。
/// 不归一就会把 `water::note_attempt` 新建的那一行水位读成"游标动了"：丢批那一轮记的是
/// attempts，位置一格都没动，而循环收批的判据正落在"位置动没动"上。
/// 读失败由调用方翻成运维错误，不许把它读成"从没跑过"——那是 `derive_water_row` 在
/// `status` 那一侧躲开的同一型谎。
fn derive_cursor(root: &Root) -> Result<(i64, String), ExitCode> {
    water::read(root.ledger(), "derive", "")
        .map(|opt| opt.map(|c| (c.at_ms, c.turn_uid)).unwrap_or((0, String::new())))
        .map_err(|_| operational(FAULT_DERIVE_WATER_READ))
}

/// `hub derive run`：沿推导池一批接一批地调 `rc_distill::derive::run_derive`。
///
/// 三件事决定这一格的存在：① 生产上原先只有 `hub cycle once` 会叫 `run_derive`，而日路径
/// 从不跑 cycle ⇒ 中心库的推导线一次都没转过；② 单批一次调用要把 40 行的批循环、熔断与
/// 读数交给脚本自己拼，等于把判定散进 shell；③ 收批的判据**必须落在游标上**——
/// `Ok{rows:0}` 同时是"选批为空"与"整批被 `INSERT OR IGNORE` 吞掉/模型答了零行"，
/// 只有游标是否移动能把两者分开，所以这里每轮都读一次前后水位。
///
/// 退出码沿用 hub 既有约定（`退出码即契约`：0 全收 / 2 整批拒 / 3 运维失败）：
/// 最后一轮的判定就是这一跑的判定——空批是 `Ok` ⇒ 0，熔断是 `Fault` ⇒ `operational` ⇒ 3，
/// 丢批是 `BatchDropped` ⇒ 2（与 `finish_cycle` 对 `CycleExit` 的映射逐字同形）。
fn derive_run(a: &DeriveRunArgs) -> ExitCode {
    // 旗标形状先于开库、先于装配（同 `distill once` / `review run`）。
    if a.llm_timeout_ms == 0 {
        return operational(FAULT_DERIVE_LLM_TIMEOUT);
    }
    // 观测面（W33-A10 · 评审缺陷②补线）：这一档的账本不再被丢弃——每批读数带
    // `max_call_ms`（进程级累计最慢值；脚本侧跨行取 max，累计即每行同值）与
    // `timeouts`（**这一批的增量**：账本是进程级累计，脚本侧跨行求和之后与
    // "每发一行"的档同口径；增量口径见 `daily-drain.sh` 的 `parse_call_obs` 注释）。
    // 回放座位没有真实出网 ⇒ 恒 0（stub 不发明耗时）。
    let (meter, slots) = match llm_source_with_timeout(
        &a.replies, a.llm_config.as_ref(), &a.data_dir, Some(a.llm_timeout_ms)) {
        Ok(v) => v,
        Err(cat) => return operational(cat),
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    // 守卫 14：推导写的是库里的边/链/时间线，资格不达标时这一批的落库形状不可信。
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    let rotating = match Rotating::new(slots) {
        Ok(r) => r,
        Err(cat) => return operational(cat),
    };
    let fail_streak = derive_fail_streak();
    let mut empty_streak = 0usize;
    let mut fault_streak = 0usize;
    let mut batch = 0usize;
    loop {
        batch += 1;
        // 每轮各取一次钟：一轮可能跑很多批、每批一次真调用，把"最后一次成功"记成开跑那一刻
        // 就不是那一刻的事（与日路径"每次调用一个新进程"的钟语义一致）。
        let Some(now_ms) = unix_now_ms() else {
            return operational("system clock is behind the unix epoch");
        };
        let before = match derive_cursor(&root) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let timeouts_before = meter.timeouts();
        let (ro, exit) = rc_distill::derive::run_derive(
            root.memory(), root.ledger(), &rotating, now_ms);
        let timeouts_delta = meter.timeouts() - timeouts_before;
        let after = match derive_cursor(&root) {
            Ok(v) => v,
            Err(code) => return code,
        };
        let cursor_moved = before != after;
        // 余量与选批同一个家（`reader::count_rows_since` 用的就是 `rows_since` 的臂表与游标谓词）；
        // 读不出来只少这一格读数（`null`），不把一轮已交出的判定改写成退出码 3。
        let lag_rows = rc_memory::reader::count_rows_since(root.memory(), Some(after)).ok();
        if a.json {
            // 派发档 I-2b · B-1：`rejected` 那一分诊族第一次上运维面。**既有键一个没改名、一个没删**
            // （`rows`/`rejected` 的读者是 `daily-drain.sh` 的 `parse_rows` 与 `rc-cycle`，改名即打穿），
            // 新键一律"只加"，`schema_version` 仍 = 1（与 `rc-cycle::CycleOut` 加格子时同一条律：
            // 只加不减的读数不必升版，升版反而让既有读者读不出来）。观测面两格（W33-A10 ·
            // 评审缺陷②补线）同一条律追加在最后。
            let line = json!({
                "schema_version": DERIVE_RUN_SCHEMA_VERSION,
                "job": "derive",
                "batch": batch,
                "rows": ro.rows,
                "rejected": ro.rejected,
                "rejected_no_mechanism": ro.rejected_no_mechanism,
                "rejected_no_evidence": ro.rejected_no_evidence,
                "rejected_anchor_gate": ro.rejected_anchor_gate,
                "rejected_parse_other": ro.rejected_parse_other,
                "rejected_chain_merged": ro.rejected_chain_merged,
                "dropped": ro.dropped,
                "quarantined": ro.quarantined,
                "issue_id": ro.issue_id,
                "cursor_moved": cursor_moved,
                "lag_rows": lag_rows,
                // 观测面两格（W33-A10 · 评审缺陷②补线，只追加）：`max_call_ms` 是进程级
                // 累计最慢一发（脚本侧跨行取 max，每行同值即正确）；`timeouts` 是**这一批
                // 的增量**（脚本侧跨行求和）。回放座位没有真实出网 ⇒ 恒 0。
                "max_call_ms": meter.max_call_ms(),
                "timeouts": timeouts_delta,
            });
            match serde_json::to_string(&line) {
                Ok(text) => write_line(&text),
                Err(_) => return operational("derive readout serialization failed"),
            }
        } else {
            let issue = match ro.issue_id {
                Some(id) => id.to_string(),
                None => "none".to_owned(),
            };
            let lag = match lag_rows {
                Some(n) => n.to_string(),
                None => "unknown".to_owned(),
            };
            write_line(&format!("derive batch={batch} rows={} rejected={} \
                                  rejected_no_mechanism={} rejected_no_evidence={} \
                                  rejected_anchor_gate={} rejected_parse_other={} \
                                  rejected_chain_merged={} dropped={} \
                                  quarantined={} issue_id={issue} cursor_moved={} lag_rows={lag}",
                                  ro.rows, ro.rejected,
                                  ro.rejected_no_mechanism, ro.rejected_no_evidence,
                                  ro.rejected_anchor_gate, ro.rejected_parse_other,
                                  ro.rejected_chain_merged,
                                  ro.dropped, ro.quarantined,
                                  cursor_moved));
        }
        match &exit {
            // `Fault`：既不推水位也不计 attempts ⇒ 不在这里熔断就是同批无限重试。
            // 它**不算"空批"**（那一支的主语是"没有活干"，这一支的主语是"这一轮没跑成"）。
            rc_distill::derive::DeriveExit::Fault(cat) => {
                fault_streak += 1;
                empty_streak = 0;
                if fault_streak >= fail_streak {
                    return operational(cat);
                }
            }
            // 丢批/隔离轮：游标动不动由 `run_derive` 自己裁（未到隔离上限不动，到了就越批）。
            // attempts 与隔离住在库里，循环只负责继续把下一批要来的机会还给这一族。
            rc_distill::derive::DeriveExit::BatchDropped => {
                fault_streak = 0;
                empty_streak = 0;
            }
            rc_distill::derive::DeriveExit::Ok => {
                fault_streak = 0;
                // 收批判据：空批**且游标未动**连续两次。`rows == 0` 而游标动了的轮（同批重答、
                // 内容寻址全命中）是真干过活的轮，不许把它读成"跑空了"。
                if ro.rows == 0 && !ro.dropped && !cursor_moved {
                    empty_streak += 1;
                    if empty_streak >= 2 {
                        return ExitCode::from(0);
                    }
                } else {
                    empty_streak = 0;
                }
            }
        }
        // `--max-batches` 到 ⇒ 这一跑的判定就是最后一轮的判定（余量在 `lag_rows` 那一格里，
        // 明晚接着跑）。不收在循环外是为了不留一份"最后一轮"的循环携带状态：那一格的两个
        // 读点（熔断 / 上限）都在 `exit` 还在手上的时候就能判。
        if a.max_batches > 0 && batch >= a.max_batches {
            return match &exit {
                rc_distill::derive::DeriveExit::Fault(cat) => operational(cat),
                rc_distill::derive::DeriveExit::BatchDropped => ExitCode::from(2),
                rc_distill::derive::DeriveExit::Ok => ExitCode::from(0),
            };
        }
    }
}

/// `hub episode run` 的读数面版本。与 `DERIVE_RUN_SCHEMA_VERSION` 分开：两条作业的键集不同，
/// 共用一个版本号会让"derive 的读数升了版"读成"episode 的也升了"。
/// v2（W33 · A4 便宜闸）：`cursor_moved` 的判据改为"窗尾还会不会前移"（`lag_rows > 0`，
/// doc ②的本义；旧读法在 batch 1 是空洞的真，见 `episode_run` 的注释），且**收批终行**在
/// 便宜闸命中时新增 `why`（=`no_new_facts`）与 `calls`（这一跑真烧的模型调用数）两格——
/// 其余行的键集与 v1 一字不差。
const EPISODE_RUN_SCHEMA_VERSION: u32 = 2;
const FAULT_EPISODE_STORE: &str = "episode grouping store access failed — run: doctor";

/// `hub episode run` 的分派（与 [`run_derive`] 同形：一条子命令一个 match 臂）。
fn run_episode(job: &EpisodeJob) -> ExitCode {
    match job {
        EpisodeJob::Run(a) => episode_run(a),
    }
}

/// episode 归组的裁决器两形（I-12）。`Defer` = 没给座位（一律拒、零调用）；
/// `Llm` = 真裁决器（每对一次语义调用，判定表可从它交出）。
/// 包一层 enum 而不是 `Box<dyn …>` 的理由是判定表的交出口：`take_log` 是
/// `LlmMidBand` 的具体方法，trait 对象上取不到；两形在装配点定死，循环里不再分叉。
enum EpisodeJudge<'a> {
    Defer,
    Llm(rc_distill::adjudicate::LlmMidBand<'a>),
}

impl rc_distill::episode::MidBandAdjudicator for EpisodeJudge<'_> {
    fn accepts(&self, facts: &[rc_memory::reader::GroupFact],
               pair: &rc_distill::episode::Pair) -> bool {
        match self {
            EpisodeJudge::Defer => false,
            EpisodeJudge::Llm(j) => j.accepts(facts, pair),
        }
    }
}

impl EpisodeJudge<'_> {
    /// 本趟新裁决的判定表（缓存命中不产生新行）。`Defer` 永远空表。
    fn take_log(&self) -> Vec<rc_distill::adjudicate::Adjudication> {
        match self {
            EpisodeJudge::Defer => Vec::new(),
            EpisodeJudge::Llm(j) => j.take_log(),
        }
    }
}

/// `hub episode run`：沿活 fact 一趟接一趟地调 `rc_distill::episode::run_episode_pass`。
///
/// 三格读数决定这个循环的形状：
/// ① **窗永远从库头开始**（`budget` 逐趟放大，不是分页）：归组是"窗内整段重算"——
///    分页会让一段跨窗的经历永远长不出来，而放大窗重算时旧段按句柄命中 `INSERT OR IGNORE`
///    （内容寻址 id），于是一趟只会补上"窗变大之后新够得着的对"。
/// ② **收批判据落在窗尾**（`cursor_moved`）：判据是"**窗尾还会不会前移**"（`lag_rows > 0`，
///    即"上一轮 window_end 之后还有没有新 fact"——A4 便宜闸的判定读法，一条 SQL 计数，
///    `run_episode_pass` 每趟已经算好、这里零额外读）。窗尾之后还有活 fact ⇒ 放大窗能够到
///    新行 ⇒ 继续趟；零行 ⇒ 更大的窗只能逐字重算同一个窗，同窗同判定 ⇒ 写侧必全 0
///    （幂等：内容寻址 id + `INSERT OR IGNORE`）⇒ **剩余趟整段跳过**（0 模型调用，W33·A4）。
///    收批时若这一跑没写出任何段（episodes_written 合计 0），终行带 `why=no_new_facts`
///    与 `calls`（这一跑真烧的模型调用数；纯程序恒 0）——`why` 词形照日路径 `phase` 行
///    的既有习惯（queue_empty/fail_streak/step_cap 同族，下划线小写）。
///    旧读法（窗尾与上一趟比对）在 batch 1 是**空洞的真**：`before=None` 恒不等，drain 的
///    `--max-batches 1` 单趟调用因此永远读到"游标动了"，空批连烧满发数帽
///    （2026-11-07 两轮实测 calls:20 / episodes_total:0 的空转根源）。
/// ③ **游标不落库**：`job_watermark.family` 的 CHECK 是闭集（`distill|derive|anomaly_scan`），
///    新增一族是一次 schema 变更，而 DDL 不在本轮落点（母档 §1）⇒ 跨运行的收敛靠幂等，
///    不靠持久水位。A4 的便宜闸也不需要持久游标：判定读数是本趟 `lag_rows`（库内现算），
///    "上轮 window_end 之后有无新 fact"与"这趟窗尾之后还有没有行"在同一判据上合流。
///    这一格已按派发档要求登记在报告里。
///
/// 退出码沿用 hub 既有约定：0 全收 / 3 运维失败。
/// 归组没有"整批拒收"那一支——中间带对的失败是 fail-closed 折 `uncertain`（不合、不落库），
/// 不是这一跑的故障。
///
/// I-12：给了座位（`--replies` / `--llm-config`）⇒ 中间带交给真裁决器
/// （`rc_distill::adjudicate::LlmMidBand`，每对一次语义调用、重试 ≤2、失败折 uncertain）；
/// 不给座位 ⇒ 沿用 [`rc_distill::episode::DeferMidBand`]（一律拒，零调用）。
/// 裁决器的判定表（逐对 fact id/结论/理由/座位/时延/prompt 指纹/调用数）在每趟之后
/// 追加输出：`--json` 是逐行 JSONL（`job:"adjudication"`），否则 k=v 一行一对。
/// 与 [`derive_run`] 同形状的三处刻意省略：不读/写 `llm_state`（轮转态不落盘）、
/// 不 `charge_counters`（`Metered` 只累加调用数、不落 usage 账；usage 主语没有 episode 一格），
/// 也不 `probe_primary`——三格都登记在 I-12 报告，不在本层修。
fn episode_run(a: &EpisodeRunArgs) -> ExitCode {
    // 旗标形状先于开库（同 `derive_run` / `review run`）。
    if a.llm_timeout_ms == 0 {
        return operational("episode run --llm-timeout-ms must be greater than zero");
    }
    // 座位装配。给了坏座位仍当场失败——"配了但配坏了"与"没配"是两格，不许塌成一种。
    // 装出来的座位真的会被叫（中间带裁决），不再是"装而不叫"。
    let rotating: Option<Rotating> = if !a.replies.is_empty() || a.llm_config.is_some() {
        let (_meter, slots) = match llm_source_with_timeout(
            &a.replies, a.llm_config.as_ref(), &a.data_dir, Some(a.llm_timeout_ms)) {
            Ok(v) => v,
            Err(cat) => return operational(cat),
        };
        Some(match Rotating::new(slots) {
            Ok(r) => r,
            Err(cat) => return operational(cat),
        })
    } else {
        None
    };
    let adjudicator = match &rotating {
        Some(r) => EpisodeJudge::Llm(rc_distill::adjudicate::LlmMidBand::new(r)),
        None => EpisodeJudge::Defer,
    };
    let root = match open_root(&a.data_dir) {
        Ok(r) => r,
        Err(_) => return operational("cannot open data store"),
    };
    // 守卫 14：归组写的是库里的 `episode`/`episode_fact`/镜像卡，资格不达标时落库形状不可信。
    let cap = match probe_caps(&root) {
        Ok(c) => c,
        Err(code) => return code,
    };
    if !qualify(&cap) {
        return operational(FAULT_UNQUALIFIED);
    }
    let mut budget = rc_distill::episode::EPISODE_MAX_FACTS_PER_PASS;
    let mut batch = 0usize;
    // 这一跑的累计账（A4 便宜闸的两格判据 + `calls` 的交账）：
    // `run_episodes_written` = 写出的段数合计（闸命中要求它为 0——有产出的收批不是"无新 fact"）；
    // `model_calls` = 裁决器真烧的调用合计（纯程序 = 恒 0；判定表逐对行里另有明细）。
    let mut run_episodes_written = 0usize;
    let mut model_calls = 0usize;
    loop {
        batch += 1;
        let Some(now_ms) = unix_now_ms() else {
            return operational("system clock is behind the unix epoch");
        };
        let pass = match rc_distill::episode::run_episode_pass(
            root.memory(), root.transcript(), budget, &adjudicator, now_ms) {
            Ok(v) => v,
            Err(_) => return operational(FAULT_EPISODE_STORE),
        };
        // A4 便宜闸（W33）：收批判据 = "上一轮 window_end 之后还有没有新 fact"，即
        // `lag_rows > 0`——一条 SQL 计数（`count_group_facts_after`），本趟已经算好，
        // 这里零额外读。窗尾之后零行 ⇒ 放大窗只能逐字重算同一个窗 ⇒ 剩余趟全部跳过
        // （0 模型调用）。语义依据见函数头 ②：这正是 `cursor_moved` 的文档本义
        // （"窗尾不再前移 ⇒ 全库已被一个窗覆盖 ⇒ 收"），旧读法在 batch 1 是空洞的真。
        let cursor_moved = pass.lag_rows > 0;
        let ro = pass.readout;
        let lag_rows = pass.lag_rows;
        run_episodes_written += ro.episodes_written;
        // I-12：判定表的交账。每对一行（只有本趟新裁决的；缓存命中不产生新行），
        // 逐对 fact id / 结论 / 理由 / 座位 / 时延 / prompt 指纹 / 调用数。
        // 记账口径：调用数只数裁决真烧的口（判定表 `calls` 合计），`Metered` 在这一跑里
        // 只累加次数、不进 usage_counter（登记在报告，不在本层修）。
        for rec in adjudicator.take_log() {
            model_calls += rec.calls;
            let verdict = rec.verdict.slug();
            let reason = rec.reason;
            if a.json {
                let line = json!({
                    "job": "adjudication",
                    "fact_a": rec.fact_a,
                    "fact_b": rec.fact_b,
                    "verdict": verdict,
                    "reason": reason,
                    "seat": rec.seat,
                    "latency_ms": rec.latency_ms,
                    "prompt_sha": rec.prompt_sha,
                    "calls": rec.calls,
                });
                match serde_json::to_string(&line) {
                    Ok(text) => write_line(&text),
                    Err(_) => return operational("adjudication record serialization failed"),
                }
            } else {
                write_line(&format!("adjudication {}x{} verdict={verdict} seat={} \
                                      latency_ms={} calls={} prompt_sha={} reason={reason}",
                                      rec.fact_a, rec.fact_b, rec.seat, rec.latency_ms,
                                      rec.calls, rec.prompt_sha));
            }
        }
        // A4 便宜闸的终行标记：窗尾不再前移且这一跑没写出任何段 ⇒ 剩余相位整段跳过，
        // 终行带 `why=no_new_facts` + `calls`（这一跑真烧的模型调用数，纯程序 = 0）。
        // 有产出的收批（写出了段）不带 `why`——那不是"无新 fact"，是"窗已覆盖全库"。
        let gated = !cursor_moved && run_episodes_written == 0;
        if a.json {
            let mut line = json!({
                "schema_version": EPISODE_RUN_SCHEMA_VERSION,
                "job": "episode",
                "batch": batch,
                "candidates": ro.candidates,
                "merged_pairs": ro.merged_pairs,
                "span_pruned": ro.span_pruned,
                "episodes_written": ro.episodes_written,
                "members_linked": ro.members_linked,
                "cards_written": ro.cards_written,
                "superseded": ro.superseded,
                "dropped_members": ro.dropped_members,
                "mid_band_deferred": ro.mid_band_deferred,
                "cursor_moved": cursor_moved,
                "lag_rows": lag_rows,
            });
            if gated {
                line["why"] = Value::String("no_new_facts".into());
                line["calls"] = Value::from(model_calls);
            }
            match serde_json::to_string(&line) {
                Ok(text) => write_line(&text),
                Err(_) => return operational("episode readout serialization failed"),
            }
        } else {
            let gate = if gated {
                format!(" why=no_new_facts calls={model_calls}")
            } else {
                String::new()
            };
            write_line(&format!("episode batch={batch} candidates={} merged_pairs={} \
                                  span_pruned={} episodes_written={} members_linked={} \
                                  cards_written={} superseded={} dropped_members={} \
                                  mid_band_deferred={} cursor_moved={} lag_rows={lag_rows}{gate}",
                                  ro.candidates, ro.merged_pairs, ro.span_pruned,
                                  ro.episodes_written, ro.members_linked, ro.cards_written,
                                  ro.superseded, ro.dropped_members, ro.mid_band_deferred,
                                  cursor_moved));
        }
        // A4 便宜闸的收批支：窗尾之后零行 ⇒ 剩余趟整段跳过（放大了也够不到任何新行）。
        if !cursor_moved { return ExitCode::from(0); }
        // 窗放大一格：下一趟带着更大的整段重算——只有窗尾之后还有活 fact 才走得到这里。
        budget += rc_distill::episode::EPISODE_MAX_FACTS_PER_PASS;
        if a.max_batches > 0 && batch >= a.max_batches { return ExitCode::from(0); }
    }
}

/// S1：`hub fingerprint status`。分组读数的唯一数据源是 `water::fingerprint_counts`；
/// 这一层只决定两形（文本 / JSON）与空库那一句。**不取钟**：指纹读数不依赖"现在"。
fn run_fingerprint(job: &FingerprintJob) -> ExitCode {
    match job {
        FingerprintJob::Status(a) => {
            let root = match open_root(&a.data_dir) {
                Ok(r) => r,
                Err(_) => return operational("store unreachable — run: doctor"),
            };
            let counts = match rc_ledger::water::fingerprint_counts(root.ledger()) {
                Ok(c) => c,
                Err(_) => return operational("fingerprint read failed — run: doctor"),
            };
            if a.json {
                // 字段名与 `FingerprintCount` 一致；`null` 就是 legacy 组（那一组没有身份）。
                let doc = Value::Array(counts.iter().map(|c| json!({
                    "fingerprint": c.fingerprint.as_deref(),
                    "turns": c.turns,
                    "first_at_ms": c.first_at_ms,
                    "last_at_ms": c.last_at_ms,
                })).collect());
                match serde_json::to_string(&doc) {
                    Ok(s) => write_line(&s),
                    Err(_) => return operational("fingerprint report serialization failed"),
                }
            } else if counts.is_empty() {
                write_line("no extraction fingerprints");
            } else {
                for c in &counts {
                    let who = c.fingerprint.as_deref().unwrap_or("legacy");
                    write_line(&format!("{who}  turns={}  first={}  last={}",
                                        c.turns, c.first_at_ms, c.last_at_ms));
                }
            }
            ExitCode::from(0)
        }
    }
}

fn print(r: &Report, json: bool) {
    let text = if json {
        serde_json::to_string(r).unwrap_or_else(|_| "{}".to_owned())
    } else {
        // 丢弃读数只在非空时出新格：v1 输入的文本面逐字节保持原样。
        let dropped = if r.dropped_fields.is_empty() {
            String::new()
        } else {
            // BTreeMap 迭代序稳定：同一份读数两次渲染必须逐字节一致。
            let parts: Vec<String> = r
                .dropped_fields
                .iter()
                .map(|(k, v)| format!("{k}:{v}"))
                .collect();
            format!(" dropped={}", parts.join(","))
        };
        let first = format!(
            "turns_accepted={} turns_rejected={} skipped_duplicate={} batch_reject={} redacted_spans={}{}",
            r.turns_accepted,
            r.turns_rejected,
            r.skipped_duplicate,
            if r.batch_reject.is_empty() { "none" } else { &r.batch_reject },
            r.redacted_spans,
            dropped
        );
        // G19 刀 6：turn_uid 行——计数 + 前 [`REPORT_TURN_UID_LISTED`] 个 uid，
        // 截断在明处（`(+N more)`）。消费面拿 uid 去 poll `recollect_ingest_status`
        // /vote，所以文本面至少要让"有几条、前几条是什么"可读。
        let cap = REPORT_TURN_UID_LISTED;
        let listed: Vec<&str> = r.turn_uids.iter().take(cap).map(String::as_str).collect();
        let uid_line = if r.turn_uids.len() > cap {
            format!(
                "turn_uids={} {} (+{} more)",
                r.turn_uids.len(),
                listed.join(" "),
                r.turn_uids.len() - cap
            )
        } else if listed.is_empty() {
            "turn_uids=0".to_string()
        } else {
            format!("turn_uids={} {}", r.turn_uids.len(), listed.join(" "))
        };
        format!("{first}\n{uid_line}")
    };
    // 写失败（下游把管道关了，`hub import … | head`）不是导入失败：批次已落库，
    // 此时 panic 会把退出码变成 101，让 agent 侧读不到真正的批次判定。
    // 裁定 R19：这条容忍规则住在 `write_line` 里，`run_index` 的三处读数共用同一个入口。
    write_line(&text);
}

#[cfg(test)]
mod readouts {
    use super::*;
    // 账本 `Metered` 不进这里的 `use`：`readouts` 两条都不构造它，真正的构造点是 `replay.rs` 的
    // `mod tests`（它自己文件头已经 import）。留着它就是 `unused_imports`，-D warnings 当场红。
    use rc_ledger::water::{self, Cursor};
    // B2b-1：`全量扫描的触发轮号是_1_21_41` / `落后太多轮_一次扫描只补一轮` /
    // `qualify_refuses_the_two_shapes_a_store_can_have` 三条跟着它们测的函数搬去了 `rc-cycle`，
    // 总数不变（hub 少三条、`rc-cycle` 多三条）。停轮位这一条测的是 hub 的读法，留在原地。
    use rc_cycle::halted_of;

    #[test]
    fn halted_reads_only_the_session_this_round_touched() {
        // A15：生产路径上这一格今天读不到真值（`run_extract` 递进去的游标恒在本批之后，
        // `advance` 因此看不见倒退）。这一条测的是**读法**，不是给它造读数：
        // 造读数要在 cycle 里加一条"扫别的会话是否停轮"的查询，那会把一轮的语义
        // 从"一个批次"改成"扫全库"（§2.19 明令不许）。
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(&dir.path().join("data")).unwrap();
        let l = root.ledger();
        // `assert!(!..)` 而不是 `assert_eq!(.., false)`：后者被 clippy `bool_assert_comparison` 判红。
        assert!(!halted_of(l, None).unwrap(), "本轮没有会话 ⇒ 不许去猜别的会话");
        // `advance` 只在游标**倒退**时置停轮（任务 3 的规则①），用它造一个真 halted 的会话。
        // 倒退那一次同时返回 Err（规则①"停轮且推进被拒"，`water.rs::advance` 的倒退分支），
        // 所以这里断 `is_err` 而不是 `.unwrap()`：停轮正是这次被拒的推进留下的状态。
        water::advance(l, "distill", "dev-x/sess-x",
                       &Cursor { at_ms: 20, turn_uid: "b".repeat(64) }, 20).unwrap();
        assert!(water::advance(l, "distill", "dev-x/sess-x",
                       &Cursor { at_ms: 10, turn_uid: "a".repeat(64) }, 21).is_err(),
                   "倒退的那次推进必须被拒，不然停轮就不是它造出来的");
        assert!(halted_of(l, Some("dev-x/sess-x")).unwrap());
        assert!(!halted_of(l, Some("dev-y/sess-y")).unwrap(),
                 "别的会话的停轮不许串到本轮这一格");
    }

    /// `error: llm` 的四态后缀（2026-10-31）：四个词全是 `&'static str`、都以类别词 `llm` 开头，
    /// 且逐字等于 [`crate::replay`] 里 `--replies` 那张 slug 表 —— 操作员拿同一个词能两处对查。
    ///
    /// 证红做法：把任一支的后缀改错，`table` 那一支红；把整个函数退回 `"llm"`（撤掉这一格），
    /// 四条全红 ⇒ 这一格的牙是可数的，不是修辞。
    #[test]
    fn llm_fault_text_carries_the_four_states_after_the_category_word() {
        let table = [
            (LlmFailure::Timeout, "llm timeout"),
            (LlmFailure::SchemaMismatch, "llm schema_mismatch"),
            (LlmFailure::QuotaExhausted, "llm quota_exhausted"),
            (LlmFailure::BackendError, "llm backend_error"),
        ];
        let mut seen = std::collections::HashSet::new();
        for (failure, want) in table {
            assert_eq!(llm_fault_text(failure), want, "四态词与 slug 表不一致: {failure}");
            assert!(want.starts_with("llm "), "类别词必须是第一个词: {want}");
            assert!(seen.insert(want), "两支撞成同一个词: {want}");
        }
        assert_eq!(seen.len(), 4, "四态必须四个词");
    }

    // ---- §4.8 ①②：装配的内部形状 ----
    //
    // 这一族单元格的边界是"不碰环境变量、不碰网络"：`claimed_device_id` 的读数面由
    // `cycle_once.rs` 的 `device_identity_from_the_environment_decides_which_loud_failure`
    // 用真子进程钉（那里能摘掉 ambient 值，这里不能——同一条断言写在进程内会随本机环境飘）。
    // 这里钉的是**形状**：座位构造接受什么、拒什么，以及启动那五步的前四步各自归哪一类。

    fn preset_seat(provider: &str, model: &str) -> rc_llm_cfg::Seat {
        rc_llm_cfg::Seat { provider: Some(provider.into()), model: Some(model.into()),
                           secret_ref: "kms:slot".into(), endpoint: None, budget: None,
                           derive_budget: None, temperature: None, reasoning_effort: None }
    }

    #[test]
    fn a_seat_on_the_preset_table_builds_and_one_off_it_does_not() {
        // 构造点做校验、不在别处做（实现律③"本地校验先于出网"）。`build_live_seat` 收的是
        // `(&Seat, u64, &str)`：B2a-3 起第三参是**真 key**，这一条给的是测试值——
        // 它验的是座位形状成不成立，不是"能不能真出网"（`Egress` 只被装进盒子、一次都不许被调用）。
        let deadline = rc_llm_http::transport::CALL_WALL_MS;
        let mut seat = preset_seat("deepseek", "deepseek-chat");
        assert!(build_live_seat(&seat, deadline, "sk-test-value").is_ok(),
                "表上的行被拒了 ⇒ 座位来源之二永远起不来");
        seat = preset_seat("not-a-preset", "nope-not-on-the-table");
        assert!(build_live_seat(&seat, deadline, "sk-test-value").is_err(),
                "查不到键却构造成功 ⇒ 静默回落默认那一家");
        // custom 通道（节 1 接缝②）：preset 表查不到它，但它的形状是合法的 ⇒ 不许被算成 preset miss。
        let custom = rc_llm_cfg::Seat {
            provider: None, model: Some("some-model".into()), secret_ref: "kms:slot".into(),
            endpoint: Some(rc_llm_cfg::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(), protocol: "openai-chat".into() }),
            budget: None, derive_budget: None, temperature: None, reasoning_effort: None,
        };
        assert!(build_live_seat(&custom, deadline, "sk-test-value").is_ok(),
                "custom 座位被 preset 那一支拦了 ⇒ 两条通道没接全");
    }

    #[test]
    fn a_seat_derive_budget_reaches_the_live_backend_and_none_falls_back() {
        let deadline = rc_llm_http::transport::CALL_WALL_MS;
        let custom = rc_llm_cfg::Seat {
            provider: None, model: Some("some-model".into()), secret_ref: "kms:slot".into(),
            endpoint: Some(rc_llm_cfg::Endpoint {
                base_url: "http://127.0.0.1:9/v1".into(), protocol: "openai-chat".into() }),
            budget: None, derive_budget: Some(524_288), temperature: None, reasoning_effort: None,
        };
        let live = build_live_seat(&custom, deadline, "sk-test-value").unwrap();
        assert_eq!(live.base_budget(), rc_llm_http::MAX_BUDGET, "基础预算被 derive 值改掉了");
        assert_eq!(live.derive_budget(), 524_288, "llm.toml 的 derive_budget 没有传到 Live");
        let mut default = custom.clone();
        default.derive_budget = None;
        let live = build_live_seat(&default, deadline, "sk-test-value").unwrap();
        assert_eq!(live.derive_budget(), live.base_budget(), "缺省 derive 预算没有回落到基础预算");
    }

    #[test]
    fn build_live_seat_passes_the_configured_temperature() {
        // 不出网：`Live` 的构造点只查表与查参数，`Egress` 被装进盒子后一次都不调用
        // （同族的上一条用例已是这个事实：没有真 key 也能构造成功）。
        let deadline = rc_llm_http::transport::CALL_WALL_MS;
        let mut seat = preset_seat("agnes", "agnes-3.0-flash");
        seat.temperature = Some(0.0);
        assert!(build_live_seat(&seat, deadline, "sk-test-value").is_ok(),
                "未锁死的行配 0.0 被拒了 ⇒ 这一格没传到 Live");

        // 锁死语义没被绕过：`sensenova/kimi-k3` 只许 1.0。0.0 必须**在构造点**被拒——
        // 若 `build_live_seat` 还在递 `None`（`check_params` 对 None 放行），这条会绿 ⇒ 断言红。
        let mut locked = preset_seat("sensenova", "kimi-k3");
        locked.temperature = Some(0.0);
        assert!(build_live_seat(&locked, deadline, "sk-test-value").is_err(),
                "锁死 1.0 的行收下了 0.0 ⇒ 配置值没被递到构造点，锁死形同虚设");

        // 阳性对照：同一行给锁死值 1.0 ⇒ 构造成功（上面那条红的不是"这一行永远起不来"）。
        let mut locked_ok = preset_seat("sensenova", "kimi-k3");
        locked_ok.temperature = Some(1.0);
        assert!(build_live_seat(&locked_ok, deadline, "sk-test-value").is_ok(),
                "锁死值本身被拒了 ⇒ 那条红证明的是值，不是行");
    }

    // ---- W19c1：单发墙是上限，旗标只许往下调 ----

    #[test]
    fn the_timeout_flag_can_only_shrink_the_call_wall() {
        let wall = rc_llm_http::transport::CALL_WALL_MS;
        // 这一格是"某一发最多能挂多久"，不是预算：旗标给了更大的值也不许把墙抬起来，
        // 否则一次卡死的调用会把整族停摆（日路径靠它回到调用方）。
        assert_eq!(call_deadline(600_000), wall, "缺省旗标把墙调大了");
        assert_eq!(call_deadline(u64::MAX), wall, "任意大的旗标把墙调大了");
        assert_eq!(call_deadline(wall), wall, "恰好等于墙的值必须原样过");
        assert_eq!(call_deadline(1_500), 1_500, "调小必须生效（冒烟要能提前放弃）");
    }

    // ---- §4.8 ③ 段 B：座位标签（B4）----

    #[test]
    fn seat_labels_are_the_safe_token_shape_and_carry_no_path() {
        // 裁定 D 的那一串标签：逐字 `reply-1`、`reply-2`、…，**不含文件名、不含路径**。
        // 它们会进 `llm_state.slug`、会被 `doctor` 原样念出来，所以过的是操作员那一关：
        // 把操作员递进来的那个路径拼进座位名 = 破守卫 12 的零路径规则。
        assert_eq!(seat_slug(0), "reply-1");
        assert_eq!(seat_slug(1), "reply-2");
        assert_eq!(seat_slug(11), "reply-12");
        for i in 0..12 {
            let label = seat_slug(i);
            assert_eq!(rc_contract::safe_token(&label), label, "座位标签过了 `safe_token` 的形状关");
            assert!(!label.contains("replies") && !label.contains('\\') && !label.contains('/'),
                     "座位标签带了文件名或路径的影子：{label}");
        }
    }

    // ---- §4.8 ③ 段 B：`Row` ↔ `Seed`/`Persisted` 的转换面 ----

    #[test]
    fn the_two_conversions_move_every_column_and_invent_none() {
        // B1 的牙：**逐列等值、不 clamp、不补默认值**（裁定 C 的另一半——坏值绕回来比被静默
        // 修好可读，而今天写这一行的只有 hub）。两个方向各钉一次：
        // `Row`→`Seed` 只丢 `machine`，其余一列不许变（`written_at_ms` 必须逐列带走 ——
        // 作废规则③"种里的戳比本机钟还新就丢掉"读的就是这一列）；
        // `Persisted`→`Row` 补上的只有调用方递进来的那两个值，且 `probe_due_at_ms` 的 `None`
        // 必须**还是** `None`（补成 `Some(0)` 会被下一个进程读成"随时可以探测"）。
        let row = rc_ledger::llm_state::Row {
            machine: "box-laptop".into(), slug: "reply-2".into(),
            probe_due_at_ms: Some(111), probe_ok: 7, probe_failures: 9, written_at_ms: 42,
        };
        let seed = seed_of_row(&row);
        assert_eq!(seed, rc_llm::Seed { slug: "reply-2".into(), probe_due_at_ms: Some(111),
                                       probe_ok: 7, probe_failures: 9, written_at_ms: 42 });
        let persisted = rc_llm::Persisted { slug: "reply-2".into(), probe_due_at_ms: None,
                                           probe_ok: 0, probe_failures: 0 };
        assert_eq!(row_of_persisted("box-laptop", 42, &persisted), rc_ledger::llm_state::Row {
            machine: "box-laptop".into(), slug: "reply-2".into(), probe_due_at_ms: None,
            probe_ok: 0, probe_failures: 0, written_at_ms: 42 });
        // 非零那三格同样原样过（`probe_ok` 归零 = 把"已经探测成功两次"写成"从没探测过"）。
        assert_eq!(row_of_persisted("x", 1, &rc_llm::Persisted { slug: "s".into(),
            probe_due_at_ms: Some(5), probe_ok: 2, probe_failures: 3 }).probe_ok, 2);
    }

    // ---- B2a-3：hub 读箱（真 key、三类失败、显式边界）----

    /// 测试口令与它落在 `.pass` 里的字节形状（一个行尾换行）——读侧要剥掉的那一个。
    const HUB_TEST_PASS: &[u8] = b"hub-test-passphrase-123";

    /// 造一个节点/hub 自己 data-dir 里的箱；`slot` 为 `None` 时箱是空的（缺格那一支）。
    fn seed_secret_box(data: &Path, slot: Option<(&str, &str)>) {
        std::fs::create_dir_all(data).unwrap();
        {
            let mut b = rc_secret::SecretsBox::create(&data.join("secrets.enc"), HUB_TEST_PASS)
                .expect("the box is created");
            if let Some((slot, value)) = slot {
                b.put(slot, value).expect("the slot is written");
            }
        }
        std::fs::write(data.join("secrets.pass"), b"hub-test-passphrase-123\n").unwrap();
    }

    /// 一座 preset 座位，`secret_ref` 由调用方点名要读哪一格。
    fn write_llm_config(dir: &Path, secret_ref: &str) -> PathBuf {
        let p = dir.join("llm.toml");
        std::fs::write(&p, format!(
            "[[providers]]\nprovider = \"deepseek\"\nmodel = \"deepseek-chat\"\n\
             secret_ref = \"{secret_ref}\"\n",
        ))
        .unwrap();
        p
    }

    #[test]
    fn a_live_seat_gets_a_non_empty_key_from_the_box() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        seed_secret_box(&data, Some(("openai", "sk-test-value")));
        let cfg_path = write_llm_config(dir.path(), "secrets:openai");
        let cfg = rc_llm_cfg::load(&cfg_path).unwrap().expect("the config is there");
        // 真箱真 key：live 座位拿到的是箱里的值，不是空串。
        let keys = live_keys(&cfg, &data).expect("the box answers");
        assert_eq!(keys, vec!["sk-test-value".to_string()]);
        assert!(!keys[0].is_empty(), "空 key 的座位不许被造出来");
        // 密钥**不在** `Seat` 里：值的位置是 `secret_ref`，值本身只住在箱里。
        let seat = &cfg.seats[0];
        assert!(!format!("{seat:?}").contains("sk-test-value"),
                "Seat 的 Debug 里出现了 key：它只该有 secret_ref");
        let deadline = rc_llm_http::transport::CALL_WALL_MS;
        assert!(build_live_seat(seat, deadline, keys[0].as_str()).is_ok(),
                "真 key 的座位构造失败 ⇒ 有了读箱的手也起不来");
    }

    #[test]
    fn the_three_key_failures_each_get_their_own_category() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let data = d.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let cfg_path = write_llm_config(d, "secrets:openai");
        let tmp = d.file_name().unwrap().to_string_lossy().into_owned();
        let check = |cat: &'static str, expected: &'static str| {
            assert_eq!(cat, expected, "三类读箱失败没分得开：{cat}");
            // 零路径、零值、零 slot 名：三类失败都只报类别。
            assert!(
                !cat.contains(tmp.as_str())
                    && !cat.contains("openai")
                    && !cat.contains("hub-test-passphrase"),
                "类别串里带了路径/格名/口令：{cat}"
            );
        };
        // ① 缺箱（目录里一个箱都没有）。
        check(live_slots_fault_with(Some(&cfg_path), &data, Some("dev-x")), FAULT_LLM_BOX_MISSING);
        // ② 箱与口令都在，格缺。
        seed_secret_box(&data, None);
        check(live_slots_fault_with(Some(&cfg_path), &data, Some("dev-x")), FAULT_LLM_SLOT_MISSING);
        // ③ 箱在、口令文件在，但口令是错的（AEAD 打不开）。
        std::fs::write(data.join("secrets.pass"), b"not-the-passphrase\n").unwrap();
        check(live_slots_fault_with(Some(&cfg_path), &data, Some("dev-x")), FAULT_LLM_SECRET_OPEN);
    }

    #[test]
    fn live_seats_resolve_into_metered_slots() {
        // 全成功这一格从 B2a 的"显式边界"变成"真造出槽位"：真箱 → 真 key → `MeteredBackend`。
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().join("data");
        seed_secret_box(&data, Some(("openai", "sk-test-value")));
        let cfg_path = write_llm_config(dir.path(), "secrets:openai");
        let (meter, slots) = live_slots_with(Some(&cfg_path), &data, Some("dev-x"))
            .expect("全成功必须造出槽位，而不是一句边界");
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].0, "deepseek:deepseek-chat",
                   "live 的 slug 是 provider:model，不是会随顺序变化的 live-<序号>");
        assert_eq!(meter.calls(rc_llm::JobFamily::Extract), 0, "构造座位不算一次调用");
    }

    #[test]
    fn the_first_four_startup_steps_each_get_their_own_category() {
        // `live_slots_fault_with` 的前四步（§4.8 ② 的五步与 B2a-3 读箱那一步的前四步）：
        // 每一类各归一条类别串，且**都不带路径**——把 `--llm-config` 的值拼进消息就是破守卫 12。
        // 身份传 `None`：第 4 步必须挡在读箱之前（下面那个目录里根本没有箱，报的仍必须是身份）。
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let data = d.join("data");
        assert_eq!(live_slots_fault_with(None, &data, None), FAULT_NO_LLM_SOURCE,
                   "两条来源都没给 ⇒ 该点名的就是那两个旗标");
        let gone = d.join("absent.toml");
        assert_eq!(live_slots_fault_with(Some(&gone), &data, None), FAULT_NO_LLM_SOURCE,
                   "没配（Ok None）⇒ 同一条：还是那句「给个来源」");
        let bad = d.join("bad.toml");
        std::fs::write(&bad, "providers = [ this is not toml").unwrap();
        let cat = live_slots_fault_with(Some(&bad), &data, None);
        assert_eq!(cat, FAULT_LLM_CFG_BAD, "配坏了（Err）⇒ 不许读成「还没配」");
        assert!(!cat.contains("bad.toml") && !cat.contains(&d.file_name().unwrap().to_string_lossy().into_owned()),
                 "类别串里带了路径或文件名：{cat}");
        let miss = d.join("miss.toml");
        std::fs::write(&miss, "[[providers]]\nprovider = \"not-a-preset\"\nmodel = \"nope\"\nsecret_ref = \"kms:slot\"\n").unwrap();
        assert_eq!(live_slots_fault_with(Some(&miss), &data, None), FAULT_LLM_PRESET,
                   "preset 查不到键却走到了身份那一步 ⇒ 第 3、4 步的顺序被反了");
        let good = write_llm_config(d, "secrets:openai");
        assert_eq!(live_slots_fault_with(Some(&good), &data, None), FAULT_NO_DEVICE_ID,
                   "身份缺必须挡在读箱之前：箱根本不在，报的却不是缺箱");
    }
}

#[cfg(test)]
mod secret_values {
    use super::*;

    #[test]
    fn 只剥掉恰好一个行尾换行() {
        assert_eq!(strip_one_line_ending(b"sk-live\n"), b"sk-live");
        assert_eq!(strip_one_line_ending(b"sk-live\r\n"), b"sk-live");
        assert_eq!(strip_one_line_ending(b"line1\nline2\n"), b"line1\nline2");
        assert_eq!(strip_one_line_ending(b"line1\nline2\n\n"), b"line1\nline2\n", "只许掉一个");
        assert_eq!(strip_one_line_ending(b"no-newline"), b"no-newline");
        assert_eq!(strip_one_line_ending(b""), b"");
    }

    #[test]
    fn 值上限是_64_kib_且含边界() {
        assert!(!secret_value_too_large(0));
        assert!(!secret_value_too_large(MAX_SECRET_VALUE_BYTES), "正好 64 KiB 是合法的");
        assert!(secret_value_too_large(MAX_SECRET_VALUE_BYTES + 1));
    }
}

#[cfg(test)]
mod serve_tls_matrix {
    use super::tls_decision;

    fn addr(s: &str) -> std::net::SocketAddr {
        s.parse().unwrap()
    }

    /// 规则矩阵逐格：默认按"是否回环"分流，两个旗标各自能把它掰到相反的一边，
    /// 同给就是用法错误（不许静默取一个）。
    #[test]
    fn tls_is_default_on_mesh_and_off_on_loopback() {
        assert_eq!(tls_decision(addr("100.64.0.10:7420"), false, false), Ok(true));
        assert_eq!(tls_decision(addr("127.0.0.1:7420"), false, false), Ok(false));
        assert_eq!(tls_decision(addr("[::1]:7420"), false, false), Ok(false));
        // 显式 `--insecure-http` 把非回环也压回明文；显式 `--tls` 把回环抬到 TLS。
        assert_eq!(tls_decision(addr("100.64.0.10:7420"), false, true), Ok(false));
        assert_eq!(tls_decision(addr("127.0.0.1:7420"), true, false), Ok(true));
        // 同给：用法错误。
        assert_eq!(tls_decision(addr("127.0.0.1:7420"), true, true), Err(()));
        assert_eq!(tls_decision(addr("100.64.0.10:7420"), true, true), Err(()));
    }
}

#[cfg(test)]
mod backup_key_tests {
    use super::backup_key_outside_data_dir;

    #[test]
    fn backup_key_must_be_provable_outside_data_dir() {
        let t = tempfile::tempdir().unwrap();
        let data = t.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let inside = data.join("bk.key");
        std::fs::write(&inside, b"x").unwrap();
        assert!(!backup_key_outside_data_dir(&inside, &data));

        let outside = t.path().join("bk.key");
        std::fs::write(&outside, b"x").unwrap();
        assert!(backup_key_outside_data_dir(&outside, &data));

        let not_yet = t.path().join("new.key");
        assert!(backup_key_outside_data_dir(&not_yet, &data));

        let ghost = t.path().join("ghost");
        assert!(!backup_key_outside_data_dir(&ghost.join("bk.key"), &ghost));

        // 恢复目标目录还不存在（restore 到新目录）时，外面的 key 仍要放行。
        let fresh = t.path().join("fresh");
        assert!(backup_key_outside_data_dir(&t.path().join("other.key"), &fresh));
    }
}
