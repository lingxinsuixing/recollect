//! 推导在 `run_cycle_once` 里的入口（裁定 10）。本模块只做**接线**：选批、推游标、记 attempts、
//! 把结果打成 (readout, exit)。`rc_derive::derive::derive_pass` 的写与解析全住 rc-derive。
//! 导入清单里**一个都不许多**：本仓的门禁是 `cargo clippy --workspace --all-targets` 零告警，
//! 未用的 `use` 就是红。`issues` 与 `MAX_ATTEMPTS` 是隔离路径的两个新零件（上限复用双审队列
//! 的立值，不在本文件重写一个字面量 3）。
use rc_derive::derive::{derive_pass, DeriveOutcome};
use rc_derive::parse::DeriveError;
use rc_derive::select::select_batch;
use rc_llm::{LlmBackend, LlmFailure};
use rc_ledger::water::{self, Cursor};
use rc_memory::{issues, review::MAX_ATTEMPTS};
use rc_store::{LedgerHandle, MemoryHandle};

/// 推导家族一轮最多算多少行（裁定 6）。值与 `rc_derive::select::select_batch` 的 `limit` 同源，
/// 一边改后另一边跟着改是 P-77 那一族——裁决记在 §1.2 的一条字面量。
pub const DERIVE_MAX_ROWS_PER_BATCH: usize = 40;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeriveReadout {
    pub rows: usize,
    pub rejected: usize,
    /// 契约丢批 = `true`：本轮整批拒收（与 extract 那一支同形）。`run_cycle_once`
    /// 据此决定 `batches_dropped` vs `derive_dropped` 的归并方式。
    pub dropped: bool,
    /// 丢批的封闭类别（今天只有 `schema`）。没有它，`stderr` 只能说"丢了"，
    /// 运维分不出缺键与格式错；有正文/路径的原串一律不进这一格（守卫 12）。
    pub drop_reason: Option<&'static str>,
    // 批次 M 段 C · mention 三格（段 B 落在 `DeriveOutcome` 的三个计数器送到 cycle 的 JSON 读数面）：
    // `mentions_missing` 是"缺第四键"的唯一可观测出口，`mentions_unresolved` 是"名字解析不到实体"的计数。
    // 契约丢弃 / Fault / 空批的 `DeriveReadout::default()` 保持零——那一轮没有派生写入。
    pub mentions_written: usize,
    pub mentions_unresolved: usize,
    pub mentions_missing: usize,
    /// 本轮隔离的批数（0/1）。`dropped` 说的是"整批没落行"，这一格说的是"这一批被
    /// 越过并且留下了一条可重跑的 issue"——两者在 `attempts` 到上限之前不是同一件事。
    pub quarantined: usize,
    /// 本轮 quarantine 落下的 `extraction_issue.id`；没隔离 ⇒ `None`。
    /// 运维面按库里的那一行点名（`hub derive status` 看未解决数、`hub derive retry --issue`
    /// 拿 detail 回拨）；这一格让读 `run_derive` 的调用方一眼看到"这一轮隔离了哪一条"。
    pub issue_id: Option<i64>,
    // ── 派发档 I-2b · B-1：`rejected` 的分诊五格（与 `DeriveOutcome` 同名同口径）───────
    // `rejected` 那一格**一字未动**（P-5：`rc-cycle/src/lib.rs` 的 `run_cycle_once` 正在累加它，改名就打穿下游），
    // 这五格是把同一批拒收**摊开**：`rejected_no_mechanism` / `rejected_no_evidence` 来自 parse 的
    // 两支码，`rejected_anchor_gate` 来自 `put_edges_anchored` 的第二格（I4 门槛），
    // `rejected_chain_merged` 来自 `put_chains_merged` 的第二格（I5 合并/复述），
    // `rejected_parse_other` 是"枚举外 kind、别名越界…"那些**没有码**的单条拒收。
    // 恒等式：`rejected == 五格之和`（`derive_pass` 末尾的 `debug_assert` + hub 的那条用例）。
    // **有意的不对称**（登记在报告里）：`hub cycle once` 的 JSON 面（`rc-cycle::CycleOut`）仍然只有
    // `derive_rejected` 一格——那一处不在本轮落点内，母档 P-5 也禁改，于是两个读数面的粒度不同。
    pub rejected_no_mechanism: usize,
    pub rejected_no_evidence: usize,
    pub rejected_anchor_gate: usize,
    pub rejected_parse_other: usize,
    pub rejected_chain_merged: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DeriveExit {
    /// 这一轮跑完并推了游标。**`rows == 0` 是合法读数**：三个写入口返回的都是"这次真落了几条"
    /// 而不是计划条数，所以模型把同一批内容又答了一遍（内容寻址 id + `INSERT OR IGNORE` 全命中）、
    /// 或这一批的行全被 parse 单条拒收（计 `rejected`、不落行）时，`rows_written` 就是 0，
    /// 而批确实被处理过了。
    /// **这句原先不是这么写的**：批次③ 段 B 之前 chains 那一族根本没有写入口，"模型只给 chains"
    /// 那一轮的 0 是**丢弃**换来的；段 B 落了 `put_chains` ⇒ 同样一句答复现在真会落库，
    /// 那个丢弃出口连同它的读数一起没了（工单 §4 步 4 明写不留兼容壳）。
    Ok,
    /// 整批被拒（parse 失败 / 缺键）。**`ZeroOutput` 不在这一支**：四键齐全、四数组全空
    /// 是模型对"没有可推导内容"的合格答复，走 `Ok` 并推游标（`rows=0`）。
    BatchDropped,
    /// 运维失败（**类别串只许是 `&'static str`**；守卫 12）
    Fault(&'static str),
}

const FAULT_STORE: &str = "store write failed — run: doctor";
const FAULT_READ:  &str = "store read failed — run: doctor";

/// 事实批契约丢弃时调用方**不调**本函数——T5 落点的"四类不补"之一。
/// 单独提出来是为了让 `run_cycle_once` 的插入点能写成 `if !contract_discarded { … }`。
pub fn run_derive(m: &MemoryHandle, l: &LedgerHandle, backend: &dyn LlmBackend, now_ms: i64)
    -> (DeriveReadout, DeriveExit)
{
    // 1. 读游标（family='derive'、scope_key=''；列名是中性 `at_ms`，
    //    本族的轴仍是记忆行 `created_at`——换轴只换了 distill 那一族）
    let from = match water::read(l, "derive", "") {
        Ok(opt) => opt.map(|c| (c.at_ms, c.turn_uid)),
        Err(_) => return (DeriveReadout::default(), DeriveExit::Fault(FAULT_READ)),
    };
    // 2. 选批（**只取活行**——裁定 6 在 rc-memory/reader::rows_since 那一侧执法；本层不重判）
    //    `from` 留一份：契约丢弃的隔离路径要把"选批时的游标"写进 issue 的 detail。
    let batch = match select_batch(m, from.clone(), DERIVE_MAX_ROWS_PER_BATCH) {
        Ok(v) => v,
        Err(_) => return (DeriveReadout::default(), DeriveExit::Fault(FAULT_READ)),
    };
    if batch.is_empty() {
        // 没活行 ⇒ 不调 backend、不推游标（与 P-32 活锁规则同律）
        return (DeriveReadout::default(), DeriveExit::Ok);
    }
    // 3. 跑 pass（段 0 已于 T6c 上提到 `hub::run_cycle_once`，**不在本 pass 里**）
    let outcome: DeriveOutcome = match derive_pass(m, &batch, backend, now_ms) {
        Ok(o) => o,
        // `ZeroOutput` 是**四键齐全、四数组全空**：模型明确答了"这批没有可推导的内容"。
        // 它与缺键/格式错不同——后者是答复不合格，前者是合格的空答复。空答复照推游标
        // （`rows=0` 是合法读数），否则 cron 摘要这类真的没有关系可推的批会永久被丢、
        // attempts 无限增长（P1 实测到的形状）。
        Err(DeriveError::ZeroOutput) => DeriveOutcome {
            rows_written: 0, rejected: 0, mentions_written: 0,
            mentions_unresolved: 0, mentions_missing: 0,
            // 合格的空答复：既没落行也没拒收，五格分计与总格同为 0（恒等式不退化）。
            rejected_no_mechanism: 0, rejected_no_evidence: 0, rejected_anchor_gate: 0,
            rejected_parse_other: 0, rejected_chain_merged: 0,
        },
        // 契约族 ⇒ 整批丢：游标不动 + attempts +1（§6.4-3 同律）。
        // **到上限就隔离并越过这一批**：同一个坏批不再每轮重烧额度（P-32 的 live-lock）。
        Err(DeriveError::Contract(_)) => {
            // 水位库写失败**上抛**为 `Fault`，与 hub `run_cycle_once` 的契约丢弃那一支同形
            // （那里是 `if water::note_attempt(…).is_err() { return …::Fault(FAULT_STORE_WRITE) }`）。
            // 本支原先写的是 `let _ = water::note_attempt(…)`——**吞掉**库错误，于是水位写失败时
            // 这一轮照样报 `BatchDropped` 而 `attempts` 没涨 ⇒ 活锁在运维面上完全不可见，
            // 正是裁定 12 / P-32 要防的那件事。同族失败两种处置不可留。
            if water::note_attempt(l, "derive", "").is_err() {
                return (DeriveReadout::default(), DeriveExit::Fault(FAULT_STORE));
            }
            // 读回 attempts：`note_attempt` 只涨不读，而上限判定必须看**这一次**记完之后的数。
            let attempts = match water::attempts(l, "derive", "") {
                Ok(n) => n,
                Err(_) => return (DeriveReadout::default(), DeriveExit::Fault(FAULT_READ)),
            };
            if attempts < MAX_ATTEMPTS {
                return (DeriveReadout { dropped: true, drop_reason: Some("schema"),
                                        ..Default::default() },
                        DeriveExit::BatchDropped);
            }
            // ── 隔离：先把记录落下，再越批。顺序有意：反过来的话 `advance_settled` 失败就是
            // "批被永远跳过、却没有一条 issue"，那正是本工单要消灭的不可见性。
            let last = batch.last()
                .expect("run_derive：batch 非空由第 2 步的提前返回保证");
            let cur = Cursor { at_ms: last.created_at_ms, turn_uid: last.key.key_string() };
            // `None` 游标 = "从没推过"，等价于初值 (0, "")；`retry_from` 回到它就是从出生点重来。
            let (prev_at_ms, prev_uid) = from.unwrap_or((0, String::new()));
            // `turns` 只放 turn_uid（哈希身份），最多与批上限同量级。出处读不出来不是隔离理由：
            // 它是诊断线索，缺了就少一格，不许把一次已判定的隔离退回到 Fault。
            let mut turns: Vec<String> = Vec::new();
            let mut seen = std::collections::BTreeSet::new();
            'collect: for row in &batch {
                let Ok(list) = rc_memory::reader::provenance_of(m, &row.key) else { continue };
                for (uid, _) in list {
                    if seen.insert(uid.clone()) {
                        turns.push(uid);
                        if turns.len() >= DERIVE_MAX_ROWS_PER_BATCH {
                            break 'collect;
                        }
                    }
                }
            }
            // 守卫 12：detail 只许轻量元数据——没有 title/body、没有路径、没有模型原文。
            let detail = serde_json::json!({
                "attempts": attempts,
                "prev_at_ms": prev_at_ms,
                "prev_uid": prev_uid,
                "batch_last_at_ms": cur.at_ms,
                "batch_last_uid": &cur.turn_uid,
                "turns": turns,
            }).to_string();
            let issue_id = match issues::record(m, issues::DERIVE_PASS, issues::DERIVE_SCHEMA_CODE,
                                                &issues::derive_run_id(now_ms), &detail, now_ms) {
                Ok(id) => id,
                Err(_) => return (DeriveReadout::default(), DeriveExit::Fault(FAULT_STORE)),
            };
            // 与成功路径同一个 `Cursor`，同一个"结算"写口：`attempts` 归零，否则这条线会把
            // 隔离过的坏批攒下的数带进下一批，偶发失败迟早误伤好批。
            if water::advance_settled(l, "derive", "", &cur, now_ms).is_err() {
                return (DeriveReadout::default(), DeriveExit::Fault(FAULT_STORE));
            }
            return (DeriveReadout { dropped: true, drop_reason: Some("schema"),
                                    quarantined: 1, issue_id: Some(issue_id),
                                    ..Default::default() },
                    DeriveExit::BatchDropped);
        }
        // LLM 故障族 ⇒ 不推水位、不计 attempts（hub `run_cycle_once` 的契约丢弃那一支同律）。四支措辞与
        // hub 的 `cycle_failure` 逐字相同：同一族失败在抽取与推导
        // 两条作业上必须念同一句话（两处字面量本阶段不合并——合并要改 hub，不在本任务文件集内）。
        Err(DeriveError::Llm(f)) => return (DeriveReadout::default(),
            DeriveExit::Fault(match f {
                LlmFailure::Timeout => "llm timeout — retry this round later",
                LlmFailure::SchemaMismatch => "llm reply shape mismatch — run: doctor",
                LlmFailure::QuotaExhausted => "llm quota exhausted — run: doctor",
                LlmFailure::BackendError => "llm backend unavailable — check the service",
            })),
        // 库故障族 ⇒ 报错、不计 attempts。**这里没有 `_` 兜底支**：`DeriveError` 以后新增变体时
        // 必须在本 match 编译失败，否则新失败会被静默念成 "store write failed"
        // （hub 的 `cycle_failure` 写最后那一支，挡的正是同一型）。
        Err(DeriveError::Store(_)) => return (DeriveReadout::default(), DeriveExit::Fault(FAULT_STORE)),
    };
    // 4. 推游标：用最后一行 created_at_ms + key_string（不是 turn_uid——D8 原文）。
    //    上面第 2 步的 `batch.is_empty()` 提前返回保证 `last()` 必有值，所以这里不写 None 支：
    //    一个永远不会发生的分支会让"游标推进"这一格出现一个没有生产者的读数（P-44 那一族）。
    //    走 `advance_settled` 而不是 `advance`：成功的批把 attempts 归零（见 water 那一侧的
    //    理由——attempts 是"这条线累计失败数"，不归零会让偶发失败跨批攒到上限）。
    let last = batch.last()
        .expect("run_derive：batch 非空由第 2 步的提前返回保证");
    // `key_string()` 在 T2 已落定；reader::RowFacts 上的字段名见 T2 src
    let cur = Cursor { at_ms: last.created_at_ms, turn_uid: last.key.key_string() };
    if water::advance_settled(l, "derive", "", &cur, now_ms).is_err() {
        return (DeriveReadout { rows: outcome.rows_written, rejected: outcome.rejected,
                                mentions_written: outcome.mentions_written,
                                mentions_unresolved: outcome.mentions_unresolved,
                                mentions_missing: outcome.mentions_missing,
                                ..Default::default() },
                DeriveExit::Fault(FAULT_STORE));
    }
    (DeriveReadout { rows: outcome.rows_written, rejected: outcome.rejected,
                     mentions_written: outcome.mentions_written,
                     mentions_unresolved: outcome.mentions_unresolved,
                     mentions_missing: outcome.mentions_missing,
                     rejected_no_mechanism: outcome.rejected_no_mechanism,
                     rejected_no_evidence: outcome.rejected_no_evidence,
                     rejected_anchor_gate: outcome.rejected_anchor_gate,
                     rejected_parse_other: outcome.rejected_parse_other,
                     rejected_chain_merged: outcome.rejected_chain_merged,
                     ..Default::default() },
     DeriveExit::Ok)
}
