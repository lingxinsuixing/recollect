//! `recollect-eval`：阶段 5 C1 的四把尺子，加 D1 的阈值-召回曲线。
//!
//! 这里的每一把尺子都**只读生产路径**：召回走 `rc_agent::recall`，注入走
//! `rc_search::gate::probe_view`，抽取真值对着 `rc_memory::reader` 的既成事实核账。
//! 它不实现第二套检索、不自己拼闸门判定、不跑抽取管线（本批不碰 LLM），
//! 于是"尺子量的东西"与"agent 读到的东西"是同一条代码路径——否则两边漂一个字符，
//! 读到的分数就只是这把尺子自己的意见。
//!
//! 三条纪律（架构 §10）：
//! ① ** fixtures 一律合成**：`seed` 只写 `合成-<序号>` 与合成主题词，不写真实姓名/设备名/生产文本；
//! ② **桩必须有牙**：指标是算出来的，不是从被度量的输出反推的（`tests/eval.rs` 的 4① 用手算值钉住）；
//! ③ **eval 不许被生产码 import**：`crates/*` 的依赖清单与源码里没有这把尺子，
//! 由 `eval_is_not_a_dependency_of_any_production_crate` 扫全树执法。
//!
//! 所有对外错误串都是**封闭码**（`truth_unreadable:` 一类的常量前缀），零路径、零正文；
//! `--json` 输出单对象，字段只含计数、分数与合成标题。

pub mod gold;

use rc_agent::recall::{recall, RecallRequest};
use rc_agent::AgentErrorKind;
use rc_memory::reader::{self, RecallFilter};
use rc_memory::{put_card, synth, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Verdict};
use rc_store::MemoryHandle;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::path::Path;

/// 播种下限：13 条 `synth` 底（7 + 5 + 1）+ 12 条主题卡。
/// 真值集引用 `合成-0000..0011`，所以下限不是"能跑就行"，而是"真值集指得到的东西在库里"。
pub const SEED_ROWS_MIN: usize = 25;
/// `synth::{seed, seed_cells, seed_padded}` 写下的行数：7 + 5 + 1。
const SYNTH_BASE_ROWS: usize = 13;
/// 反噪案例要的那条超长行：`row_len` 1500 token，`1024 * 1 / 1500 == 0` ⇒ 必然低分沉默。
/// 与 `synth::seed_cells` 的"超长卡"同一取值，不是这把尺子新定的阈值。
const PADDED_TOKENS: usize = 1500;
const PROJECT_KEY: &str = "proj-eval-synthetic";
const TITLE_PREFIX: &str = "合成";

/// 12 条合成主题卡的正文。每一条只带**一组**查询词，互不重叠：
/// 真值集的 recall@k 因此是"命中了哪几条"，而不是"分词器恰好怎么切"。
/// 主题词全是编造的运维词汇组合，没有一条来自真实会话。
const TOPIC_BODIES: [&str; 12] = [
    "离线 回并 批次 处理 记录",
    "离线 回并 批次 处理 记录",
    "离线 回并 批次 处理 记录",
    "索引 重建 影子 表 记录",
    "索引 重建 影子 表 记录",
    "重新 生成 答案 复用 旧 缓存 记录",
    "同步 冲突 解决 路径 记录",
    "跨库 身份 校验 指纹 记录",
    "跨库 身份 校验 指纹 记录",
    "备份 恢复 清单 保留 窗口 记录",
    "备份 恢复 清单 保留 窗口 记录",
    "双目标 构建 冷启动 读数 记录",
];

/// 尺子的错误面：**每一支都是封闭码**，载荷只有 `&'static str` 的用法提示。
/// 不接 `std::io::Error`/`StoreError` 的原文——那两类 `Display` 会回显路径（守卫 12 的同一条纪律）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    Usage(&'static str),
    TruthUnreadable,
    TruthInvalid,
    DataUnavailable,
    SeedFailed,
    IndexUnavailable,
    /// 读侧要的索引层不存在（`hub index` 从没在这份库上跑过）。与 `RecallFailed` 分开：
    /// 前者是"少跑一条命令"，后者是"故障"，混起来操作员就分不开（§10.148 现场）。
    /// 载荷是 **rc-agent 分型表里那一条 hint**（不是这里自己写的句子）⇒ 下一步动作只有一个家，
    /// 两处各自措辞迟早漂。
    IndexNotBuilt(&'static str),
    RecallFailed,
    InjectFailed,
    ExtractFailed,
    WriteFailed,
    /// gold 文件读不到或格式不对。两种都是**用法/格式错**（退出码 2），不是"量出来了但不达标"。
    Gold(gold::GoldError),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::Usage(why) => write!(f, "usage: {why}"),
            EvalError::TruthUnreadable => f.write_str("truth_unreadable: the truth file cannot be read"),
            EvalError::TruthInvalid => f.write_str("truth_invalid: the truth file does not match the ruler schema"),
            EvalError::DataUnavailable => f.write_str("data_unavailable: the memory store is missing or unreadable"),
            EvalError::SeedFailed => f.write_str("seed_failed: the synthetic library could not be written"),
            EvalError::IndexUnavailable => f.write_str("index_unavailable: the index layer could not be rebuilt"),
            EvalError::IndexNotBuilt(hint) => write!(
                f,
                "index_not_built: the read side needs the index layer — {hint}"
            ),
            EvalError::RecallFailed => f.write_str("recall_failed: the recall path returned an unexpected error"),
            EvalError::InjectFailed => f.write_str("inject_failed: the probe path returned an unexpected error"),
            EvalError::ExtractFailed => f.write_str("extract_failed: the measurement could not complete"),
            EvalError::WriteFailed => f.write_str("write_failed: the output could not be written"),
            EvalError::Gold(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EvalError {}

impl From<gold::GoldError> for EvalError {
    fn from(e: gold::GoldError) -> Self {
        EvalError::Gold(e)
    }
}

impl EvalError {
    /// 用法错误是 2；读/写失败是 1。**3 留给"量出来了，但不达标"**——那是尺子的读数，
    /// 不是故障（inject/extract 的失败分支在 CLI 里显式返回 3）。
    /// gold 的格式违规也是 2：一份读不进去的 gold 不产生读数，它只是用法错。
    pub fn exit_code(&self) -> u8 {
        match self {
            EvalError::Usage(_) | EvalError::Gold(_) => 2,
            _ => 1,
        }
    }
}

pub fn read_truth<T: DeserializeOwned>(path: &Path) -> Result<T, EvalError> {
    let raw = std::fs::read_to_string(path).map_err(|_| EvalError::TruthUnreadable)?;
    serde_json::from_str(&raw).map_err(|_| EvalError::TruthInvalid)
}

fn synthetic_scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: PROJECT_KEY.to_string() }
}

/// 行的出处：64 位十六进制（与内容寻址 id 同形），角色一律 human——合成夹具不模拟弱锚，
/// 弱锚由 `synth::seed_cells` 的"弱锚卡"单独提供。
fn one_provenance(i: usize) -> Vec<Provenance> {
    vec![Provenance { turn_uid: format!("{:064x}", i + 1), anchor_role: AnchorRole::Human }]
}

/// 确定性合成库：`synth` 打底 → 12 条主题卡 → 补到 `rows` 行 → 建索引。
///
/// 补出来的行标题是 `合成-<序号>`、正文是"中文 多词 查询"一类的合成词组；
/// 前 12 条主题卡的正文由 `TOPIC_BODIES` 给定，`recall.json` 的标题对准的就是它们。
/// 内容寻址 ⇒ 同一份库重复播种不增行、句柄不变。
pub fn seed_library(m: &MemoryHandle, rows: usize, now_ms: i64) -> Result<usize, EvalError> {
    if rows < SEED_ROWS_MIN {
        return Err(EvalError::Usage("seed needs --rows of at least 25"));
    }
    synth::seed(m, now_ms).map_err(|_| EvalError::SeedFailed)?;
    synth::seed_cells(m, now_ms).map_err(|_| EvalError::SeedFailed)?;
    synth::seed_padded(m, now_ms, PADDED_TOKENS).map_err(|_| EvalError::SeedFailed)?;
    let extra = rows - SYNTH_BASE_ROWS;
    for i in 0..extra {
        let body = match TOPIC_BODIES.get(i) {
            Some(topic) => format!("{topic} 第{i}条"),
            None => format!("中文 多词 查询 样例 第{i}条 备注"),
        };
        let card = FactContent {
            kind: "note".into(),
            title: format!("{TITLE_PREFIX}-{i:04}"),
            body,
            scope: synthetic_scope(),
            valid_to_ms: None,
        };
        put_card(m, &card, &one_provenance(i), now_ms).map_err(|_| EvalError::SeedFailed)?;
    }
    rc_index::rebuild(m, now_ms).map_err(|_| EvalError::IndexUnavailable)?;
    Ok(SYNTH_BASE_ROWS + extra)
}

// ── 召回：recall@k / precision@k / MRR ────────────────────────────────────────

#[derive(Debug, Clone, Deserialize)]
pub struct RecallTruth {
    pub queries: Vec<RecallQuery>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RecallQuery {
    pub query: String,
    /// 用**标题**对齐真值（标题唯一）。空数组 = "应无命中"，用来读沉默/噪声。
    pub relevant_titles: Vec<String>,
    /// 已知难例标记（如"重生成答案"：查询词与标题不同）。只进读数，不参与计算。
    #[serde(default)]
    pub hard_case: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct QueryScore {
    pub query: String,
    pub hard_case: bool,
    pub relevant: usize,
    pub retrieved: Vec<String>,
    pub recall_at_k: f64,
    pub precision_at_k: f64,
    pub reciprocal_rank: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct RecallOutcome {
    pub queries: usize,
    pub k: usize,
    pub recall_at_k: f64,
    pub precision_at_k: f64,
    pub mrr: f64,
    pub per_query: Vec<QueryScore>,
}

/// 从 agent 面召回里取前 k 个**标题**。`recall` 在"库里没有"时返回 `Empty`（`no_rows`）——
/// 那是沉默的读数，不是故障；其它错误一律上抛（"磁盘坏了"不许被读成"没有记忆"）。
fn retrieve_titles(m: &MemoryHandle, query: &str, k: usize, now_ms: i64) -> Result<Vec<String>, EvalError> {
    let request = RecallRequest::Query(query.to_string());
    match recall(m, &request, &RecallFilter::empty(), now_ms) {
        Ok(json) => {
            let value: serde_json::Value =
                serde_json::from_str(&json.text()).map_err(|_| EvalError::RecallFailed)?;
            let rows = value.get("rows").and_then(|r| r.as_array())
                .ok_or(EvalError::RecallFailed)?;
            Ok(rows.iter().take(k).filter_map(|row| {
                row.get("title").and_then(|t| t.as_str()).map(str::to_owned)
            }).collect())
        }
        Err(e) if e.kind == AgentErrorKind::Empty => Ok(Vec::new()),
        // 读侧自己会说"索引没建"（`rc-agent/src/error.rs` 的分型表是唯一的家），尺子不许把它压成
        // "unexpected error"：那是两条不同的下一步（等一次重建 vs 查故障）。
        Err(e) if e.code == "index_not_built" => Err(EvalError::IndexNotBuilt(e.hint)),
        Err(_) => Err(EvalError::RecallFailed),
    }
}

/// 三条指标的唯一算法。`|relevant| == 0` 的那一条查询（"应无命中"）按**沉默命中率**记：
/// 一条也没召回 ⇒ 1（该沉默时沉默了），召回了噪声 ⇒ 0。它不参与"分母是 relevant"的除法，
/// 也就不会把 0/0 折成 1 去抵消别处的漏召；precision 仍按公式 `0/k = 0` 如实报低。
pub fn measure_recall(m: &MemoryHandle, truth: &RecallTruth, k: usize, now_ms: i64)
    -> Result<RecallOutcome, EvalError> {
    if k == 0 {
        return Err(EvalError::Usage("--k must be at least 1"));
    }
    if truth.queries.is_empty() {
        return Err(EvalError::TruthInvalid);
    }
    let mut per_query = Vec::with_capacity(truth.queries.len());
    let mut recall_sum = 0.0_f64;
    let mut precision_sum = 0.0_f64;
    let mut rr_sum = 0.0_f64;
    for q in &truth.queries {
        let retrieved = retrieve_titles(m, &q.query, k, now_ms)?;
        let hits = q.relevant_titles.iter()
            .filter(|title| retrieved.iter().any(|r| r == *title))
            .count();
        let recall_at_k = if q.relevant_titles.is_empty() {
            if retrieved.is_empty() { 1.0 } else { 0.0 }
        } else {
            hits as f64 / q.relevant_titles.len() as f64
        };
        let precision_at_k = hits as f64 / k as f64;
        // MRR 的秩是**前 k 里的名次**（1 起）。没有命中记 0。
        let reciprocal_rank = retrieved.iter()
            .position(|title| q.relevant_titles.contains(title))
            .map(|rank| 1.0 / (rank + 1) as f64)
            .unwrap_or(0.0);
        recall_sum += recall_at_k;
        precision_sum += precision_at_k;
        rr_sum += reciprocal_rank;
        per_query.push(QueryScore {
            query: q.query.clone(),
            hard_case: q.hard_case,
            relevant: q.relevant_titles.len(),
            retrieved,
            recall_at_k,
            precision_at_k,
            reciprocal_rank,
        });
    }
    let n = per_query.len() as f64;
    Ok(RecallOutcome {
        queries: per_query.len(),
        k,
        recall_at_k: recall_sum / n,
        precision_at_k: precision_sum / n,
        mrr: rr_sum / n,
        per_query,
    })
}

// ── 阈值：G6 的阈值-召回曲线（D1） ──────────────────────────────────────────

/// 曲线扫的十个阈值，升序。`0` 是"不过滤"，`1024` 是"只有满分才开口"。
/// 档位不是等距的：`0/1` 与 `32` 之间是"零分挡不挡"的分界，往上到 `1024` 是分数上沿。
pub const THRESHOLD_STEPS: [i64; 10] = [0, 1, 32, 64, 128, 256, 384, 512, 768, 1024];

/// 一个阈值档的宏观读数。`precision` / `recall` / `f1` 都是**逐查询先算、再取算术均值**；
/// 空相关项的查询单独计数（`silenced`），不进均值。
#[derive(Debug, Clone, Serialize)]
pub struct ThresholdPoint {
    pub t: i64,
    /// 命中数 / **放行行数**（不是 /k）。阈值收紧后放行不到 k 行时，
    /// 分母跟着放行集合走——否则"档位收紧导致少开口"会被读成"precision 掉了"，
    /// 而这里要量的是"开口的那几行有多纯"。放行 0 行 ⇒ 0（空集不给满分）。
    pub precision: f64,
    /// 命中数 / 相关项数。空相关项的查询不走这一除法（见 `silenced`）。
    pub recall: f64,
    /// `2PR/(P+R)`；`P+R == 0` ⇒ 0。
    pub f1: f64,
    /// 落进本轮宏观均值的查询数（有相关项的那些）。`rows == 0` 的档不参与选点。
    pub rows: usize,
    /// 每查询放行行数的均值（上游先截到 k），读"这一档把候选砍到多窄"。
    pub kept: f64,
    /// 空相关项查询里本档仍然沉默（放行 0 行）的条数。它们不进 precision/recall 均值，
    /// 但"该沉默时沉默了没有"仍要能读。
    pub silenced: usize,
}

/// 一条查询的排序结果：相关标题 + `(标题, 分数)` 降序。
/// 分数只在这把尺子的内存里存在，不落库、不进输出（守卫 7）。
struct RankedQuery {
    query: String,
    relevant: Vec<String>,
    rows: Vec<(String, i64)>,
}

/// 每个阈值下每条查询的明细（计划 §一："输出每个阈值的宏观均值与每条查询明细"）。
/// 文本面只打宏观表；这条明细随 `--json` 输出。空相关项查询的 `precision`/`recall` 是 `None`
/// （除法没有分母），它们唯一可读的是 `silenced`。
#[derive(Debug, Clone, Serialize)]
pub struct ThresholdQueryRow {
    pub t: i64,
    pub query: String,
    pub relevant: usize,
    pub kept: usize,
    pub hits: usize,
    pub precision: Option<f64>,
    pub recall: Option<f64>,
    /// 空相关项查询且本档放行 0 行 ⇒ `true`；有相关项的查询恒为 `false`。
    pub silenced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ThresholdOutcome {
    pub k: usize,
    pub queries: usize,
    /// 真值集里 `relevant_titles` 为空的查询数；它们不进宏观均值。
    pub no_relevant_queries: usize,
    pub thresholds: Vec<ThresholdPoint>,
    pub per_query: Vec<ThresholdQueryRow>,
    /// F1 最高的阈值，同分取**较小**的阈值（派发档 §二）。
    pub selected: i64,
    /// 比 `selected` 更松一档（阈值集合里它的前一个）；`selected` 已是最松档 ⇒ `None`。
    pub looser: Option<i64>,
}

/// 对每条真值查询走一次 `rc_search::rank::rank`（生产路径的候选池 + 全序），
/// 再按十个阈值重放同一批候选。**不在检索层过滤**（G6③）：阈值只作用于这里的
/// 放行判定，`rank` 返回的仍是全部 FTS 命中。
///
/// 标题从 `reader::facts` 的 key → title 映射取：`rank` 的 `Scored` 只带 `MemKey` 与分数，
/// 真值集按标题对齐，所以这一步是两者的接缝。标题不在映射里 ⇒ `ExtractFailed`，
/// 不静默跳过（那会把一条本该计数的行读成"从未出现"）。
pub fn measure_thresholds(m: &MemoryHandle, truth: &RecallTruth, k: usize, now_ms: i64)
    -> Result<ThresholdOutcome, EvalError> {
    if k == 0 {
        return Err(EvalError::Usage("--k must be at least 1"));
    }
    if truth.queries.is_empty() {
        return Err(EvalError::TruthInvalid);
    }
    let facts = reader::facts(m).map_err(|_| EvalError::ExtractFailed)?;
    let titles: HashMap<String, String> = facts
        .into_iter()
        .map(|f| (f.key.key_string(), f.title))
        .collect();
    let mut ranked = Vec::with_capacity(truth.queries.len());
    for q in &truth.queries {
        let hits = rc_search::rank::rank(m, None, now_ms, &q.query).map_err(|e| {
            // 两条读侧路径都过 rc-agent 那张分型表 ⇒ hint 只有一个家，不许这里再写一遍句子
            let te = rc_agent::ToolError::from(e);
            if te.code == "index_not_built" {
                EvalError::IndexNotBuilt(te.hint)
            } else {
                EvalError::RecallFailed
            }
        })?;
        let mut rows = Vec::with_capacity(hits.len());
        for hit in hits {
            let title = titles
                .get(&hit.key.key_string())
                .cloned()
                .ok_or(EvalError::ExtractFailed)?;
            rows.push((title, hit.score));
        }
        ranked.push(RankedQuery { query: q.query.clone(), relevant: q.relevant_titles.clone(), rows });
    }
    let measurable = ranked.iter().filter(|q| !q.relevant.is_empty()).count();
    if measurable == 0 {
        // 全部查询都没有相关项 ⇒ 宏观均值是 0/0。没有一条能参与除法的查询时，
        // 这张曲线没有读数可给，按真值集不合法处理，而不是造一个 0。
        return Err(EvalError::TruthInvalid);
    }
    let no_relevant_queries = ranked.len() - measurable;
    let mut points = Vec::with_capacity(THRESHOLD_STEPS.len());
    let mut per_query = Vec::with_capacity(THRESHOLD_STEPS.len() * ranked.len());
    for &t in THRESHOLD_STEPS.iter() {
        let mut precision_sum = 0.0_f64;
        let mut recall_sum = 0.0_f64;
        let mut f1_sum = 0.0_f64;
        let mut kept_sum = 0_usize;
        let mut silenced = 0_usize;
        for q in &ranked {
            // `rank` 已按分数降序，所以这里只需按阈值短路 + 截到 k。
            let kept: Vec<&(String, i64)> = q.rows.iter().filter(|(_, s)| *s >= t).take(k).collect();
            let hits = q.relevant.iter()
                .filter(|title| kept.iter().any(|(r, _)| r == *title))
                .count();
            if q.relevant.is_empty() {
                let is_silent = kept.is_empty();
                if is_silent {
                    silenced += 1;
                }
                per_query.push(ThresholdQueryRow {
                    t,
                    query: q.query.clone(),
                    relevant: 0,
                    kept: kept.len(),
                    hits: 0,
                    precision: None,
                    recall: None,
                    silenced: is_silent,
                });
                continue;
            }
            let precision = if kept.is_empty() {
                0.0
            } else {
                hits as f64 / kept.len() as f64
            };
            let recall = hits as f64 / q.relevant.len() as f64;
            let f1 = if precision + recall == 0.0 {
                0.0
            } else {
                2.0 * precision * recall / (precision + recall)
            };
            precision_sum += precision;
            recall_sum += recall;
            f1_sum += f1;
            kept_sum += kept.len();
            per_query.push(ThresholdQueryRow {
                t,
                query: q.query.clone(),
                relevant: q.relevant.len(),
                kept: kept.len(),
                hits,
                precision: Some(precision),
                recall: Some(recall),
                silenced: false,
            });
        }
        let n = measurable as f64;
        points.push(ThresholdPoint {
            t,
            precision: precision_sum / n,
            recall: recall_sum / n,
            f1: f1_sum / n,
            rows: measurable,
            kept: kept_sum as f64 / n,
            silenced,
        });
    }
    // 同分取较小阈值：升序扫描、只在**严格更大**时换人，于是平局留在前面（更松）那一档。
    let mut best = &points[0];
    for p in &points[1..] {
        if p.f1 > best.f1 {
            best = p;
        }
    }
    let selected = best.t;
    let looser = THRESHOLD_STEPS
        .iter()
        .position(|&t| t == selected)
        .and_then(|i| i.checked_sub(1))
        .map(|i| THRESHOLD_STEPS[i]);
    Ok(ThresholdOutcome {
        k,
        queries: ranked.len(),
        no_relevant_queries,
        thresholds: points,
        per_query,
        selected,
        looser,
    })
}

// ── 注入：hit / silent / anti_noise ───────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InjectKind {
    Hit,
    Silent,
    AntiNoise,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InjectCase {
    pub kind: InjectKind,
    pub query: String,
    #[serde(default)]
    pub expected_title: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InjectTruth {
    pub cases: Vec<InjectCase>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ClassRate {
    pub total: usize,
    pub passed: usize,
    pub rate: f64,
}

impl ClassRate {
    fn record(&mut self, ok: bool) {
        self.total += 1;
        if ok {
            self.passed += 1;
        }
        self.rate = self.passed as f64 / self.total as f64;
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InjectCaseOutcome {
    pub kind: InjectKind,
    pub query: String,
    pub ok: bool,
    pub candidates: usize,
    pub forms: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct InjectOutcome {
    pub cases: usize,
    pub hit: ClassRate,
    pub silent: ClassRate,
    pub anti_noise: ClassRate,
    pub ok: bool,
    pub per_case: Vec<InjectCaseOutcome>,
}

/// 三类案例**各判各的**（不共用一条串）：
/// - `hit`：期望标题出现在 `view.forms` 且 `form != Silent`；
/// - `silent`：`forms` 为空，且**候选池非空**——"什么都没检索到"不算沉默判定，
///   否则这条尺子在空库上也全绿；
/// - `anti_noise`：干扰行**不在** `forms` 里，且候选池非空（它确实进过检索，只是没开口）。
///
/// 三类必须都在真值集里出现至少一次：缺一类就是 0/0 的假满分，`ok` 直接为假。
pub fn run_inject(m: &MemoryHandle, truth: &InjectTruth, now_ms: i64) -> Result<InjectOutcome, EvalError> {
    if truth.cases.is_empty() {
        return Err(EvalError::TruthInvalid);
    }
    let mut hit = ClassRate::default();
    let mut silent = ClassRate::default();
    let mut anti_noise = ClassRate::default();
    let mut per_case = Vec::with_capacity(truth.cases.len());
    for case in &truth.cases {
        let view = rc_search::gate::probe_view(m, None, &case.query, now_ms)
            .map_err(|_| EvalError::InjectFailed)?;
        let forms: Vec<String> = view.forms.iter().map(|d| d.facts.title.clone()).collect();
        let ok = match case.kind {
            InjectKind::Hit => {
                let title = case.expected_title.as_ref().ok_or(EvalError::TruthInvalid)?;
                view.forms.iter().any(|d| {
                    &d.facts.title == title && d.form != rc_search::gate::Form::Silent
                })
            }
            InjectKind::Silent => view.candidates > 0 && view.forms.is_empty(),
            InjectKind::AntiNoise => {
                let title = case.expected_title.as_ref().ok_or(EvalError::TruthInvalid)?;
                view.candidates > 0 && !forms.iter().any(|t| t == title)
            }
        };
        match case.kind {
            InjectKind::Hit => hit.record(ok),
            InjectKind::Silent => silent.record(ok),
            InjectKind::AntiNoise => anti_noise.record(ok),
        }
        per_case.push(InjectCaseOutcome {
            kind: case.kind,
            query: case.query.clone(),
            ok,
            candidates: view.candidates,
            forms,
        });
    }
    let ok = hit.total > 0
        && silent.total > 0
        && anti_noise.total > 0
        && hit.passed == hit.total
        && silent.passed == silent.total
        && anti_noise.passed == anti_noise.total;
    Ok(InjectOutcome { cases: per_case.len(), hit, silent, anti_noise, ok, per_case })
}

// ── 抽取：期望标题 / provenance 覆盖率 / 误清率（对既有库度量，不跑 LLM） ─────

#[derive(Debug, Clone, Deserialize)]
pub struct ExtractTurn {
    pub turn_uid: String,
    pub role: String,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ExtractTruth {
    /// 合成 turn：只用来计"真值集描述了几轮"，本尺子**不**据此跑抽取管线。
    #[serde(default)]
    pub turns: Vec<ExtractTurn>,
    pub expected_titles: Vec<String>,
    pub must_keep_titles: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExtractOutcome {
    pub turns: usize,
    pub expected_total: usize,
    pub expected_found: usize,
    pub provenance_rows: usize,
    pub provenance_with_origin: usize,
    pub provenance_coverage: f64,
    pub must_keep: usize,
    pub false_clear: usize,
    pub missing_titles: Vec<String>,
    pub cleared_titles: Vec<String>,
    pub ok: bool,
}

fn push_missing(into: &mut Vec<String>, title: &str) {
    if !into.iter().any(|t| t == title) {
        into.push(title.to_string());
    }
}

/// 对**既有库**的度量，三步：
/// ① `expected_titles` 每一条都在库的行里（按 title 查 `reader::facts`）；
/// ② `provenance` 覆盖率：每一条事实行的出处链非空（`reader::provenance_chain` 是"任何出处"的口径，
///    比只数 turn 的那一支更宽——缺一条就退出 3）；
/// ③ 误清率：`must_keep_titles` 里被否决（`rejected` / `verdict=failed`）或退场（`retired`）命中的条数，
///    必须是 0。`must_keep` 指向一条库里根本没有的行，也记 `missing_titles`（消失不是"没被误清"）。
pub fn run_extract(m: &MemoryHandle, truth: &ExtractTruth) -> Result<ExtractOutcome, EvalError> {
    if truth.expected_titles.is_empty() {
        return Err(EvalError::TruthInvalid);
    }
    let facts = reader::facts(m).map_err(|_| EvalError::ExtractFailed)?;
    let mut missing_titles = Vec::new();
    let mut expected_found = 0_usize;
    for title in &truth.expected_titles {
        if facts.iter().any(|f| &f.title == title) {
            expected_found += 1;
        } else {
            push_missing(&mut missing_titles, title);
        }
    }
    let mut with_origin = 0_usize;
    for row in &facts {
        let chain = reader::provenance_chain(m, &row.key)
            .map_err(|_| EvalError::ExtractFailed)?;
        if !chain.is_empty() {
            with_origin += 1;
        }
    }
    let mut false_clear = 0_usize;
    let mut cleared_titles = Vec::new();
    for title in &truth.must_keep_titles {
        let rows: Vec<&reader::RowFacts> = facts.iter().filter(|f| &f.title == title).collect();
        if rows.is_empty() {
            push_missing(&mut missing_titles, title);
            continue;
        }
        let cleared = rows.iter().any(|f| {
            f.rejected || f.retired || f.verdict == Verdict::Failed
        });
        if cleared {
            false_clear += 1;
            cleared_titles.push(title.clone());
        }
    }
    let coverage = if facts.is_empty() {
        1.0
    } else {
        with_origin as f64 / facts.len() as f64
    };
    let ok = missing_titles.is_empty() && false_clear == 0 && with_origin == facts.len();
    Ok(ExtractOutcome {
        turns: truth.turns.len(),
        expected_total: truth.expected_titles.len(),
        expected_found,
        provenance_rows: facts.len(),
        provenance_with_origin: with_origin,
        provenance_coverage: coverage,
        must_keep: truth.must_keep_titles.len(),
        false_clear,
        missing_titles,
        cleared_titles,
        ok,
    })
}

// ── 守卫 4⑤ 的匹配器（测试与扫描共用同一份判据） ────────────────────────────

/// 一行 `Cargo.toml` 文本是否声明了对 `eval` 的依赖。
/// 认四种写法：`eval = ...`、`recollect-eval = ...`、`{ package = "eval" }`、
/// `[dependencies.eval]` 一类的表头。行内注释先剥掉——一条被注释掉的依赖不是依赖。
pub fn manifest_depends_on_eval(line: &str) -> bool {
    let code = line.split('#').next().unwrap_or_default().trim();
    if code.is_empty() {
        return false;
    }
    if let Some(section) = code.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        let section = section.trim().trim_matches('"');
        return section.ends_with(".eval")
            || section.ends_with(".\"eval\"")
            || section == "eval"
            || section == "recollect-eval";
    }
    let Some((key, value)) = code.split_once('=') else {
        return false;
    };
    let key = key.trim().trim_matches('"');
    if key == "eval" || key == "recollect-eval" || key == "recollect_eval" {
        return true;
    }
    let value = value.trim();
    value.contains("package = \"eval\"")
        || value.contains("package=\"eval\"")
        || value.contains("package = \"recollect-eval\"")
}

/// 一行 Rust 源码是否引用了尺子 crate。只认 import 形状（`eval::` / `use eval` /
/// `extern crate eval` / `recollect_eval`），不认散文里的 "eval" 子串。
pub fn source_imports_eval(line: &str) -> bool {
    let code = line.split("//").next().unwrap_or_default();
    code.contains("recollect_eval")
        || code.contains("recollect-eval")
        || contains_ident_path(code, "eval")
        || contains_ident_phrase(code, "use eval")
        || contains_ident_phrase(code, "extern crate eval")
}

/// `name::` 作为独立标识符路径出现（前一个字符不是标识符字符）。
/// 不能直接 `contains("eval::")`：`a1_eval::` 与 `recollect_eval::` 都不是尺子 crate，
/// 前者是生产 crate 里的一个普通模块名，误判会让“不许依赖 eval”的闸变成命名禁令。
fn contains_ident_path(code: &str, name: &str) -> bool {
    let needle = format!("{name}::");
    let mut start = 0;
    while let Some(i) = code[start..].find(&needle) {
        let abs = start + i;
        let before_ok = abs == 0
            || !code[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if before_ok {
            return true;
        }
        start = abs + needle.len();
    }
    false
}

/// 一个短语作为独立词出现（两侧都不是标识符字符）。
fn contains_ident_phrase(code: &str, phrase: &str) -> bool {
    let mut start = 0;
    while let Some(i) = code[start..].find(phrase) {
        let abs = start + i;
        let end = abs + phrase.len();
        let before_ok = abs == 0
            || !code[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        let after_ok = end == code.len()
            || !code[end..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_');
        if before_ok && after_ok {
            return true;
        }
        start = end;
    }
    false
}
