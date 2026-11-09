//! 推导主链（裁定 1/11/13）：逐行回读 → prompt → backend → parse → 落库。
//! 段 0（连带抑制前置扫描）**不在本文件**：它的挂点已上提到 `hub::run_cycle_once` 顶部，
//! 每轮 cycle 的第一件事，与"有没有非空推导批"无关。

use rc_llm::backend::LlmBackend;
use rc_memory::{
    RowFacts, Scope, ScopeKind,
    reader::row_participants,
    writers_x::{link_row_mentions, put_chains_merged, put_edges_anchored, put_mentions,
                put_timeline_rows_counted, ChainContent, EdgeContent, MentionContent,
                TimelineContent},
};
use rc_store::MemoryHandle;

use crate::parse::{parse_derive_output, DeriveError, REJECT_NO_EVIDENCE, REJECT_NO_MECHANISM};
use crate::prompt::{derive_system, derive_user_with_events};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeriveOutcome {
    pub rows_written: usize,
    pub rejected: usize,
    /// 实际插入的 mention 行数（`put_mentions` 的 `written`；多义名字落多行按行计）。
    pub mentions_written: usize,
    /// 零匹配的 mention 条数（名字解析不到任何实体；**不造实体**）。
    pub mentions_unresolved: usize,
    /// `mentions` 缺键或非数组 ⇒ 1（降级不丢批）；正常数组（含空数组）⇒ 0。
    pub mentions_missing: usize,
    // ── 派发档 I-2b · B-1：`rejected` 的分诊五格 ──────────────────────────────────────
    // 为什么要有这五格：`rejected` 是一格合并计数，运维看到"边少了"分不出是谁拒的（母档 T-6
    // 要的三分诊在只有总格的读数上做不到）。上面那两格**一字未动**（P-5：既有键不改名、不删，
    // `rc-cycle/src/lib.rs` 的 `CycleOut.derive_rejected` 字段正在消费它），这里只往下加。
    //
    // 五格必须**恰好拼成** `rejected`（恒等式在 `derive_pass` 末尾有 `debug_assert`，
    // 用例钉在 `crates/hub/tests/derive_run.rs::derive_run_splits_the_three_rejection_kinds_and_they_sum_to_rejected`）。
    // 为什么要留 `rejected_parse_other` 与 `rejected_chain_merged` 两格残差而不是"三类"了事：
    // `plan.rejected` 里除证据闸那两道之外还有六型无码的单条拒收（枚举外 kind / 别名越界 /
    // 环节数 < 2 / 链内重复句柄 / 时间线形态），`rejected` 里还并着 I5 的链合并数——
    // 那些判据住 `parse.rs` 与 `writers_x.rs`（本轮落点之外），既不许改名也不许把它们硬塞进
    // 三道闸的名字里（那就是"分类格与总格各说各话"）。残差成格、各自点名，恒等式才真。
    /// 证据闸码 `no_mechanism` 的条数：机制类关系交的是套话/空白机制句。
    pub rejected_no_mechanism: usize,
    /// 证据闸码 `no_evidence` 的条数：机制类关系一条依据行都没点名。
    pub rejected_no_evidence: usize,
    /// I4 关系锚门槛挡下的条数（`put_edges_anchored` 返回的第二格）。
    /// **这一格今天恰好只装锚门槛**：`plan.relationships` 的 kind 被 parse 的枚举钉死在五支
    /// 结构归属（`RELATIONSHIP_KINDS`），而门口那道证据闸只拦 `MECHANISM_RELATIONS` 那四支
    /// ⇒ derive 递进 `put_edges_anchored` 的边不可能被证据闸挡（P-4 划的界把两道的射程分开了）。
    /// 写侧那两道闸共用同一个 `dropped` 计数（`writers_x.rs` 的函数文档明写），门口分不开——
    /// 要按 relation 分诊就看这一格 ∧ `rejected_no_mechanism`/`rejected_no_evidence`。
    pub rejected_anchor_gate: usize,
    /// parse 侧**没有**证据闸码的单条拒收（`plan.rejected` 减去上面两格）。
    pub rejected_parse_other: usize,
    /// I5 链合并/复述的条数（`put_chains_merged` 返回的第二格）。
    pub rejected_chain_merged: usize,
}

/// 推导行的 P 侧署名（`source_agent` / `device_id` 两列 NOT NULL，必须有值）。
/// - `source_agent = "rc-derive"`：这是**真生产者**，不是占位——边、时间行与因果链确实是这个作业写的。
/// - `device_id = "unknown"`：库级作业没有"哪台设备"可报。`hub` 在导入面已经把不可归因
///   的设备落到同一个 `"unknown"` 主语（`hub` 的 `resolve_device` 与 `bump_rejects_by_device`
///   里那两处 `unwrap_or("unknown")`），这里沿用同一个
///   哨兵而不是新造一个词。**不写 `"synthetic-device"`**——那个形状是夹具记号（P-29 那一族），
///   生产行穿夹具的记号 = 把测试与真实数据混成同一个指纹。
///
/// 后果登记：按设备筛 recall 时，推导行归到 `unknown` 档（T14 Step 6 写台账时逐条点名）。
const DERIVE_SOURCE_AGENT: &str = "rc-derive";
const DERIVE_DEVICE_ID: &str = "unknown";

pub fn derive_pass(
    m: &MemoryHandle, batch: &[RowFacts], backend: &dyn LlmBackend, now_ms: i64,
) -> Result<DeriveOutcome, DeriveError> {
    // 段 0（连带抑制前置扫描）原先挂在这一处，**已搬走**：它在 `hub::run_cycle_once` 顶部，
    // 每轮 cycle 的第一件事。这里挂着的形状有两道结构性漏——`run_derive` 在空批上提前 return、
    // hub 在契约丢弃轮次整个不跑推导 ⇒ 空闲轮次段 0 一次都不跑；而段 0 退场的行立刻退出活行
    // 集合 ⇒ 它缩小下一轮的推导池 ⇒ 池子清空那一轮起它自己饿死，此后新死的依据再也扫不到。
    // 不挂 `apply_merges` 写入侧的理由：那一支只关心"这条新行该不该写"，不关心"既有的依据死了"
    // ——后者是 cycle 级一致性问题。
    // 段 0 的 `SuppressReport` 三格也不升成 `CycleOut` 的格子（它在 hub 侧跑，但读数不出口）：
    // `suppressed` 那一格已有生产者（hub 的抽取那一支把 `extract::ExtractOutcome.suppressed`
    // 累加进 `out.suppressed`），把段 0 的数加进去会让一格变两本账（守卫 3 同族）。
    // **按函数点名而不写行号坐标**（R89）：那一行会随 hub 接线增删漂。
    //
    // 空批是**调用方 bug**：T5 的 `run_derive` 已在 `batch.is_empty()` 上提前返回，所以这一支今天
    // 没有生产者。它绝不能折成 `ZeroOutput`——那一格的意思是"模型对非空批答了零行"，把"没人递行"
    // 报成"模型没内容"就是运维面说谎。折成零行读数（语义为真）+ debug 断言（当场炸给开发者看）。
    debug_assert!(!batch.is_empty(), "derive_pass 收到空批 = 调用方 bug（run_derive 已在空批提前返回）");
    if batch.is_empty() {
        return Ok(DeriveOutcome { rows_written: 0, rejected: 0, mentions_written: 0,
                                  mentions_unresolved: 0, mentions_missing: 0,
                                  rejected_no_mechanism: 0, rejected_no_evidence: 0,
                                  rejected_anchor_gate: 0, rejected_parse_other: 0,
                                  rejected_chain_merged: 0 });
    }
    // 段 1：拼 user 段。参与者只从事实/事件行取（卡 → pointers → fact.participants_json
    // → anchor.canonical）；取不到就渲染 who=unknown，不猜。
    let participants: Vec<Vec<String>> = batch.iter()
        .map(|r| row_participants(m, r))
        .collect::<Result<Vec<_>, _>>()
        .map_err(DeriveError::Store)?;
    // 债 #9（修法甲）：derive 池的 `at=` 同口径——card 行优先 `card.time_json` 的事件时间
    // （批量侧查 1 条 SQL；`derive_pool_union` 选出来的池行以 card 为主，锚行没有 `time_json`
    // ⇒ 恒 `None` ⇒ 回落 `as_of_ms`）。`at=` 只用于排序的语义不变（prompt 的 TAIL_DERIVE）。
    let card_ids: Vec<String> = batch.iter()
        .filter(|r| r.key.table == rc_memory::MemTable::Card)
        .map(|r| r.key.id.clone())
        .collect();
    let events = rc_memory::reader::card_event_times(m, &card_ids)
        .map_err(DeriveError::Store)?;
    let event_ms: Vec<Option<i64>> = batch.iter()
        .map(|r| if r.key.table == rc_memory::MemTable::Card {
            events.iter().find(|e| e.id == r.key.id).map(|e| e.start_ms)
        } else {
            None
        })
        .collect();
    // C5：时间线的相对日期那一支要读的是**与上面 `at=` 同一口事件钟**。这里是那一次批量
    // 读数的**第二读者**（不为它再查一遍 SQL，也不在 derive 里拼第二份 `time_json` 解析）；
    // 键是行 id，缺席 = 这一行库里没有事件时间。
    let event_start_of: std::collections::HashMap<&str, i64> = batch.iter()
        .zip(event_ms.iter())
        .filter_map(|(r, ms)| ms.map(|ms| (r.key.id.as_str(), ms)))
        .collect();
    let prompt = rc_llm::backend::Prompt {
        system: derive_system(),
        user: derive_user_with_events(batch, &participants, &event_ms),
        derive: true,
    };
    // 段 2：backend。**LLM 故障族不许折进 `Contract`**：契约族计 attempts 并标"整批丢"，而配额/
    // 超时那一支既不推水位也不计 attempts（hub 的 `cycle_failure` 同律）。
    let reply = backend.complete(&prompt).map_err(DeriveError::Llm)?;
    // 段 3：parse（收整批 `RowFacts`：mentions 的 verbatim 检查要读该行的 title/body）
    let plan = parse_derive_output(&reply, batch)?;
    // C28：一批可以跨归属（选批按全库 (created_at_ms, tbl, id) 序，同 scope 相邻不是保证），
    // 所以归属必须逐行取。`parse` 已把每个别名解析回本批的真实行 id ⇒ 下面的查找必有命中；
    // 命不中就是 parse 的承诺破了，响亮炸给开发者看，不退回 `batch[0]`（那正是本批要拆的谎）。
    let by_id: std::collections::HashMap<&str, &RowFacts> =
        batch.iter().map(|r| (r.key.id.as_str(), r)).collect();
    let row_of = |id: &str| {
        *by_id.get(id).unwrap_or_else(|| panic!("parse 把别名解析到本批行 id ⇒ {id} 必在本批"))
    };
    // P 独占列的批次上下文（守卫 11：`source_agent` / `device_id` 只能从 `BatchCtx` 进写入口，
    // 所以那两个字段在 `EdgeContent` / `TimelineContent` 上**不存在**）。
    let ctx = rc_memory::BatchCtx {
        source_agent: DERIVE_SOURCE_AGENT.into(),
        device_id: DERIVE_DEVICE_ID.into(),
    };
    // 段 4：落库
    let mut rows_written = 0usize;
    // `plan.rejected` 里已经含证据闸那两道（码在 `plan.rejected_codes`：`no_mechanism` /
    // `no_evidence`）。**这一格今天仍是读数面**：`DeriveReadout.rejected` 是"落库 + 拒收"的合并
    // 计数，parse 拒收、I4 挡下、链合并三类都归并到这里（P-5：读数只加不改，总格的口径一字未动）。
    // 派发档 I-2b · B-1 在此之上把合并计数**摊开成五格**（下面那五个 `rejected_*`）：
    // 总格不动、分格只加，`hub derive run` 的 JSONL 因此第一次能把"谁拒的"报出来。
    let mut rejected = plan.rejected;
    // 分诊①＋②：证据闸的两支码各数各的。计数**按码逐项加**，不按 `rejected_codes.len()` 摊派：
    // 两支码在同一次答复里可以各出现 N 次，而 `parse` 的承诺只是"每拒一条 push 一支"。
    // 未知码（日后新增的第三支）由下面 `rejected_parse_other` 的减法兜住 ⇒ 恒等式不会说谎。
    let mut rejected_no_mechanism = 0usize;
    let mut rejected_no_evidence = 0usize;
    for code in plan.rejected_codes.iter() {
        if *code == REJECT_NO_MECHANISM { rejected_no_mechanism += 1; }
        else if *code == REJECT_NO_EVIDENCE { rejected_no_evidence += 1; }
    }
    // 分诊④：parse 侧剩下那些**没有码**的单条拒收（枚举外 kind、别名越界、环节数 < 2、
    // 链内重复句柄、时间线形态那几道）。`saturating_sub` 不是遮丑：它保证这一格永不为负，
    // 而"码比拒收条数还多"本身由下面的 `debug_assert` 当场炸给开发者看。
    let rejected_parse_other = plan.rejected.saturating_sub(rejected_no_mechanism
                                                            + rejected_no_evidence);
    // 批次 M 段 B · 四键之一：mention **先写**。I4 的关系锚判据要看本批的共享 mention
    // （审计的 2 条"有共享 mention"边、3 条应铸未铸的别名 mention），所以顺序是
    // mention → 别名补铸 → edge，而不是原来的 edge → mention。
    let mentions_missing = plan.mentions_missing;
    let mut mention_items: Vec<MentionContent> = Vec::with_capacity(plan.mentions.len());
    for ment in plan.mentions {
        mention_items.push(MentionContent { row_id: ment.row_id, name: ment.name });
    }
    let mention_out = put_mentions(m, &mention_items, &ctx, now_ms).map_err(DeriveError::Store)?;
    // I6：别名锚补铸（题名/别名归一后出现在行正文里 ⇒ 一行 mention）。三处应铸未铸
    // （`fcf34493`/`601d6d62`/`cb38e28e`）由这一步补上，共享 mention 判据因此不再空集。
    let texts: Vec<(String, String)> = batch.iter()
        .map(|r| (r.key.id.clone(), format!("{} {}", r.title, r.body)))
        .collect();
    let auto_mentions = link_row_mentions(m, &texts, &ctx, now_ms).map_err(DeriveError::Store)?;
    let mentions_written = mention_out.written + auto_mentions;
    // `rows_written` 含 mention 实际插入行数（与"实际插入的行数"口径一致；I6 的别名补铸也算）。
    rows_written += mentions_written;
    // 边：I4 的锚门槛在写入口 `put_edges_anchored` 里（共享 mention 或逐字同实体；
    // `part_of` 只认共享 mention），未过门槛的条数进 `rejected` 读数。
    // 派发档 I-2（Phase 2.4 · 证据闸）在这一支上的落点：**模型那句"凭什么"就是 `reason`，
    // 它逐字进 `fact_link.basis`**（P-1：证据落那一格 + `role='basis'` 出处行，不落 `evidence_json`
    // ——那一格被 `put_edges_inner` 写死 `'[]'`，而 `reader::edges`/`EdgeFacts` 不读它）。
    // 门口那道闸只拦机制类 relation（P-4），而 derive 的关系答复今天只有结构归属那五支
    // ⇒ 这一支今天**不因 I-2 少落一条**（少落的是因果链那一族，见下面 `put_chains_merged` 之前
    // parse 已经拒掉的那两道）。拒收的分诊从 I-2b 起不再挤在 `rejected` 一格：
    // 这一支挡下的条数单独进 `rejected_anchor_gate`，parse 那两道进 `rejected_no_mechanism` /
    // `rejected_no_evidence`，链合并进 `rejected_chain_merged`，剩下的无码拒收进 `rejected_parse_other`。
    let mut edges: Vec<EdgeContent> = Vec::with_capacity(plan.relationships.len());
    for r in plan.relationships {
        // I4：边统一 project scope——先找 from/to/basis 里第一个 project 归属；
        // 全是实体归属时退回 from 端的 key（`put_edges_anchored` 会把 kind 强制成 Project）。
        let scope = project_scope(
            std::iter::once(&r.from_id)
                .chain(std::iter::once(&r.to_id))
                .chain(r.basis_ids.iter())
                .map(|id| row_of(id)));
        edges.push(EdgeContent {
            from_id: r.from_id, to_id: r.to_id, kind: r.kind, reason: r.reason,
            scope, basis_ids: r.basis_ids,
        });
    }
    // `rows_written` 累加的是**写入函数实际插入的行数**，不是计划条数：`INSERT OR IGNORE` 命中
    // 已有行时返回 0 ⇒ 同批重跑报 0（裁定"幂等 = 内容寻址 id + INSERT OR IGNORE"要的就是这一格）。
    let (edge_rows, edge_dropped) =
        put_edges_anchored(m, &edges, &ctx, now_ms).map_err(DeriveError::Store)?;
    rows_written += edge_rows;
    rejected += edge_dropped;
    // 分诊③：这一格就是 `edge_dropped` 本身（见 `DeriveOutcome::rejected_anchor_gate` 的文档：
    // derive 递进来的 kind 只可能是五支结构归属，门口那道证据闸打不到 ⇒ 挡下的全是 I4 锚门槛）。
    let rejected_anchor_gate = edge_dropped;
    // 时间线
    let mut tlines: Vec<TimelineContent> = Vec::with_capacity(plan.timeline.len());
    for t in plan.timeline {
        let (scope, ms, anchor_id, confidence) = if let Some(date) = &t.date {
            let aid = t.date_anchor_id.clone().expect("parse 给绝对日期落了依据行");
            let row = row_of(&aid);
            // `canonical_date` 已验过，`to_epoch_ms` 的 `None` 是不可达；
            // 仍用 `expect` 响亮炸，而不是把日期偷偷降成 unknown（那会谎报"没有日期"）。
            (row.scope.clone(), Some(crate::date::to_epoch_ms(date).expect("canonical date")),
             Some(aid), "anchored")
        } else {
            let anchor_row = t.anchor_id.as_ref().map(|aid| row_of(aid));
            // C5：相对日期的锚钟统一到**事件钟**——锚行的 `time_json` 事件时间优先，
            // 缺席回落**该行的 `as_of_ms`**，两者都没有 ⇒ 没有钟。
            // 不回落 `created_at_ms`：prompt 的 `at=` 已经是事件钟（`derive_user_with_events`），
            // 而从前这一支用的是入库钟 ⇒ 同一批输入里"模型看到的时间"与"程序写下的时间线"
            // 落在两个钟上。时间线的锚必须与模型看到的那口钟是同一个。
            // `as_of_ms == 0` 是读侧"这一行没有这一列"的既有形状（anchor 行的补值是字面量 0，
            // 与 `recall::row_json` 对 0 的处置同族）⇒ 按缺席读，不把 1970 当锚。
            let base = anchor_row.and_then(|r| {
                event_start_of.get(r.key.id.as_str()).copied()
                    .or((r.as_of_ms > 0).then_some(r.as_of_ms))
            });
            let ms = match (base, t.offset_ms) {
                (Some(base), Some(off)) => Some(base + off),
                _ => None,
            };
            let scope_row = anchor_row
                .or_else(|| t.basis_ids.first().map(|b| row_of(b)))
                .expect("parse 拒收了无锚且无依据的时间线");
            (scope_row.scope.clone(), ms, t.anchor_id.clone(),
             if ms.is_some() { "anchored" } else { "unknown" })
        };
        tlines.push(TimelineContent {
            // I5：标题 = `derive-` + 完整 `body_ref`，不截断（源语言由 prompt 约束）。
            title: format!("derive-{}", t.body_ref),
            body: t.body_ref, scope,
            time_anchor_ms: ms, time_anchor_id: anchor_id, time_confidence: confidence.into(),
            basis_ids: t.basis_ids,
        });
    }
    // R3：`event_link` 是时间线的旁路写入；计数与时间行一起进 `rows_written`
    //（`derive_rows` 读数因此看得见它，不是"写了但没人知道"）。
    let (timeline_rows, event_links) =
        put_timeline_rows_counted(m, &tlines, &ctx, now_ms).map_err(DeriveError::Store)?;
    rows_written += timeline_rows + event_links;
    // 因果链（批次③）：这一支的 `link_ids` 就是 parse 解析出来的环节顺序，写入口按它给
    // `provenance.ord`，所以"顺序读得回来"从模型答复到库里只有一条路（工单 C-3）。
    // `put_chains_merged` 返回（真落了几条链, 被判重复/复述而合并的条数）；合并数进
    // `rejected` 读数（I5 的"合并/降权"必须可数，不新增 cycle 字段）。
    let mut chains: Vec<ChainContent> = Vec::with_capacity(plan.chains.len());
    for c in plan.chains {
        let scope = project_scope(c.from_ids.iter().map(|id| row_of(id)));  // 环节有序，首个就是链头
        chains.push(ChainContent {
            mechanism: c.mechanism, link_ids: c.from_ids, basis_ids: c.basis_ids,
            causal_type: c.causal_type, scope,
        });
    }
    let (chain_rows, chain_merged) =
        put_chains_merged(m, &chains, &ctx, now_ms).map_err(DeriveError::Store)?;
    rows_written += chain_rows;
    rejected += chain_merged;
    // 分诊⑤：I5 的合并/复述数单独成格（它和"证据闸拒收"是两件事：一个是"这条链是把某张
    // card 又抄了一遍"，一个是"这条链说不出凭什么"）。
    let rejected_chain_merged = chain_merged;
    // **恒等式**（派发档 I-2b · B-1.2 的"分类格不许与总格各说各话"）：五格相加必须恰好是总格。
    // 这里是 `debug_assert` 而不是 `unwrap`：`rejected` 的口径由三段累加决定，写下来就是一次
    // 装配自检——将来谁往 `rejected` 里再加一型拒收而忘了同时加一格分计，开发态当场炸，
    // 而不是让运维面拿到一个"分计之和 < 总格"的读数（本仓"读数换源要枚举所有读者"那一族）。
    debug_assert_eq!(rejected, rejected_no_mechanism + rejected_no_evidence
                            + rejected_anchor_gate + rejected_parse_other + rejected_chain_merged,
        "derive 的拒收分计没有拼成总格 rejected：{rejected} != 分计之和");
    // 同一族的第二句，挡的是另一种形状：**有码没格**。今天两支码都有格子，所以这一句不响；
    // 将来 `parse` 加第三支码而这里没跟着加一格时，它开发态就响——而不是让那条码
    // 悄悄落进 `rejected_parse_other`，把"枚举外 kind"与"证据闸"混成同一格（残差格兜的是
    // 生产面的读数不裂，不是"分类可以偷懒"的许可证）。
    debug_assert_eq!(plan.rejected_codes.len(), rejected_no_mechanism + rejected_no_evidence,
        "有一支拒收码没有对应的分计格子：codes={:?}", plan.rejected_codes);
    // 锚角色最强（裁定 11）：边由 `put_edges` 内部取两端 provenance 的最强；时间行由
    // `put_timeline_rows` 内部取被锚行的最强（`anchor_role_of` 已经返回该行的最强值）。
    // 两处都不在调用方造角色——"读出来的值"必须有真读数来源（P-44）。
    Ok(DeriveOutcome { rows_written, rejected,
                       mentions_written,
                       mentions_unresolved: mention_out.unresolved,
                       mentions_missing,
                       rejected_no_mechanism, rejected_no_evidence, rejected_anchor_gate,
                       rejected_parse_other, rejected_chain_merged })
}

/// I4：边的归属统一 project。按 from → to → basis 的顺序找第一个 `ScopeKind::Project`；
/// 找不到（全是 entity 归属）就用首个行的 key——`put_edges_anchored` 会把 kind 强制成
/// Project，于是库里不会再有 `entity/<标题>` 的 relationship scope。
fn project_scope<'a>(rows: impl Iterator<Item = &'a RowFacts>) -> Scope {
    let mut fallback: Option<Scope> = None;
    for r in rows {
        if r.scope.kind == ScopeKind::Project {
            return r.scope.clone();
        }
        fallback.get_or_insert_with(|| r.scope.clone());
    }
    let key = fallback.map(|s| s.key).unwrap_or_default();
    Scope { kind: ScopeKind::Project, key }
}
