use rc_store::{LedgerHandle, StoreError};
use rusqlite::params;

pub const SUBJECT_KINDS: [&str; 4] = ["device", "agent_tool", "llm_job", "maintenance"];
/// S3a：最近一次抽取运行是否因配额触顶而停。**每格是"最近一次"的覆盖值**（`set_one`），
/// 不是累加：主语 = `llm_job:<pass>`，值只有 0/1。它是 `status.quota_exhausted` 的唯一来源，
/// 而 `llm_calls` 的累计从此只作成本读数——88 次历史调用与"本轮触没触顶"没有关系。
pub const QUOTA_STOPPED_METRIC: &str = "quota_stopped";
/// 每次 LLM 运行烧掉的调用次数（主语 = `llm_job:<pass>`，累加语义）。它住在**指标表这一格**里，
/// 生产者（hub 的各 pass，现为 naming/fact/event/state/lesson 五条）引这个名字而不是抄字面：
/// 字面一份、读者多份时，抄错一处就是
/// "成本读数在库里再开一格"而闸面看不出来（判据 17）。
pub const LLM_CALLS_METRIC: &str = "llm_calls";
pub const METRICS: [&str; 27] = [
    "ingest_turns_accepted",
    "ingest_turns_rejected",
    // purge 墓碑挡住的 turn：不落行、不分配提交号；与 rejected 分开，便于看复活尝试。
    "ingest_turns_tombstoned",
    // T2：整行被拒里"设备不符"的那一部分。它是 `ingest_turns_rejected` 的**子集**，
    // 两个计数器不许相加——`turns_rejected` 已经数过它们了。
    "ingest_turns_rejected_signature",
    "probe_hit",
    "probe_silent",
    "probe_timeout",
    "tool_call",
    "llm_calls",           // §9.5 ①：主语是 `llm_job` + `JobFamily::slug()`，单位是次数不是金额
    "rounds",              // 裁定 8：每轮 +1
    "last_full_scan_round", // 裁定 8：全量触发时置值
    // A45：抽取闸门的三数（主语 = llm_job:extract）
    "entities_seen",
    "kinds_rejected",
    "aliases_dropped",
    // 抽取覆盖缺口：模型对批内某些 turn 一个字都没答（既没引用、也没列 worthless）。
    // 主语 = llm_job:extract；状态行只能记 rejected，这个计数把"模型漏了"单独数出来。
    "turns_uncovered",
    // I7：只产 entity、没有 card/event/assertion 的轮次。它们不拿 `extracted` 状态
    // （状态落 `pending` + reason='entity_only'），这里是那个独立计数器的出口：
    // 「一臂一库」的读数按本库的 usage_counter 算，不与 extracted 混在一起。
    "entity_only_turns",
    // 压缩摘要（`[[compact_summary]]`）：本批见到的带标记轮次数，以及其中被模型
    // 错误列进 worthless、被我们拒收的那部分。两者都在 llm_job:extract 名下。
    "compact_summary_turns",
    "compact_summary_worthless",
    // T5-A：备份/清除/恢复三类时刻与计数，主语是 `maintenance`（T5-B 的生产者会写）。
    "backup_ok_at_ms",
    "backup_fail_at_ms",
    "purged_turns",
    "pending_redistill_turns",
    "restored_turns",
    // B5b：中心密钥最近一次导出归档的时刻（主语 = maintenance:key_archive）。
    // 它是 `set_one` 覆盖语义的第二个读者（第一个是 `last_full_scan_round`）。
    "key_archive_exported_at_ms",
    // S1b-C2b：`/v1/rows` 吸收时逐行抑制的 owner 冲突数（主语 = maintenance:ownership）。
    // 冲突的那一行不覆盖已有归属、也不整批拒收：只留 `memory_op(reason='owner_conflict')`
    // 与本计数（设计 ownership-persistence §3.4）。一次吸收批只 bump 一次。
    "ownership_conflicts",
    // S1b-C4b2a：`hub ownership backfill` 推不出 owner、落哨兵的行数
    // （主语 = maintenance:ownership）。它数的是"本次回填判为未决"的行，
    // 不数孤儿归属行（孤儿数只进 CLI 报告，不单独占一格）。
    "ownership_unresolved",
    // S3a：最近一次抽取运行是否因配额触顶而停（0/1，主语 = `llm_job:<pass>`）。
    // 与 `last_full_scan_round` 同族，是 `set_one` 覆盖语义的第三个读者。
    "quota_stopped",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageRow {
    pub subject_kind: String,
    pub subject_id: String,
    pub metric: String,
    pub count: i64,
    pub last_at_ms: i64,
}

fn check(kind: &str, metric: &str) -> Result<(), StoreError> {
    if !SUBJECT_KINDS.contains(&kind) {
        return Err(StoreError::Sql(format!("usage: 未知 subject_kind {kind}")));
    }
    if !METRICS.contains(&metric) {
        return Err(StoreError::Sql(format!("usage: 未知 metric {metric}")));
    }
    Ok(())
}

pub fn bump(
    l: &LedgerHandle,
    kind: &str,
    id: &str,
    metric: &str,
    delta: i64,
    now_ms: i64,
) -> Result<(), StoreError> {
    check(kind, metric)?;
    l.conn().execute(
        "INSERT INTO usage_counter(subject_kind, subject_id, metric, count, last_at_ms)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(subject_kind, subject_id, metric) DO UPDATE SET
           count = count + excluded.count, last_at_ms = excluded.last_at_ms",
        params![kind, id, metric, delta, now_ms],
    )?;
    Ok(())
}

/// `set` 语义：把 `usage_counter.count` 直接置为 `value`（不是累加）。
/// 与 `bump` 的区别是 SQL 用 `excluded.count` 直接覆盖、不是 `count + excluded.count`。
/// 用例：`last_full_scan_round` 在全量扫描触发时不是"加一"而是"置为本轮轮号"。
pub fn set_one(
    l: &LedgerHandle,
    kind: &str,
    id: &str,
    metric: &str,
    value: i64,
    now_ms: i64,
) -> Result<(), StoreError> {
    check(kind, metric)?;
    l.conn().execute(
        "INSERT INTO usage_counter(subject_kind, subject_id, metric, count, last_at_ms)
         VALUES(?1,?2,?3,?4,?5)
         ON CONFLICT(subject_kind, subject_id, metric) DO UPDATE SET
           count = excluded.count, last_at_ms = excluded.last_at_ms",
        params![kind, id, metric, value, now_ms],
    )?;
    Ok(())
}

pub fn read_all(l: &LedgerHandle) -> Result<Vec<UsageRow>, StoreError> {
    let mut st = l.conn().prepare(
        "SELECT subject_kind, subject_id, metric, count, last_at_ms FROM usage_counter
         ORDER BY subject_kind, subject_id, metric",
    )?;
    let rows = st
        .query_map([], |r| {
            Ok(UsageRow {
                subject_kind: r.get(0)?,
                subject_id: r.get(1)?,
                metric: r.get(2)?,
                count: r.get(3)?,
                last_at_ms: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// S3a 的唯一判定入口：**最近一次记过账的运行是否因配额触顶而停**。
///
/// "记过账"的集合 = `llm_job` 名下写过 `quota_stopped` 的主语。今天是 naming / fact / event /
/// state / cycle 五条（`time` 没有模型面，一个字节都不写）。集合之外的 pass 跑完不会更新这一格，
/// 所以读到的是集合内最近一次收尾留下的值——不是"全仓最后一次进程退出"的口径。
///
/// 三态是本函数存在的理由：`None` = 还没有任何一次运行写过这一格（不是 "false"——
/// 那是"跑过且没触顶"，一个真读数）；`Some(false)` / `Some(true)` 是运行收尾留下的覆盖值。
///
/// 为什么按主语 OR 而不是挑"最新的那一行"：上面那五条 pass 各自覆盖自己那一格，任何一格的
/// 最近一次触顶都说明系统最近确实撞过配额；`last_at_ms` 不参与比较
/// ——本机没有跨进程单调钟的保证，按秒级时间戳挑行会把两次同秒运行读出随机的答案。
///
/// **一处有界的已知限度（控制器裁定，不是行为变更）**：`cycle` 不在日路径上（日路径是
/// `distill once` + `review run`）。一次因配额触顶而停的 `cycle once` 会把
/// `llm_job:cycle:quota_stopped` 留成 1，而日路径里没有任何一步会覆盖它——于是下一次
/// `cycle` 运行之前，`status.quota_exhausted` 与 doctor 的告警会一直报"因配额而停"。
/// 这个读数**没过期**（那一格确实是 1），但结论过期了。OR 口径宁可误报一次触顶，也不许
/// 把真的触顶读丢；把 `cycle` 那格按日路径刷新需要新写点（不在本方案范围内）。
/// 读者照旧只看本函数：修的是"由哪个运行来刷新"，不是这里的判据。
pub fn quota_stopped(rows: &[UsageRow]) -> Option<bool> {
    let mut seen = false;
    let mut any = false;
    for r in rows.iter().filter(|r| {
        r.subject_kind == "llm_job" && r.metric == QUOTA_STOPPED_METRIC
    }) {
        seen = true;
        any |= r.count != 0;
    }
    seen.then_some(any)
}

pub fn assert_no_body_shape(l: &LedgerHandle) -> Result<Vec<String>, StoreError> {
    let mut st = l
        .conn()
        .prepare("SELECT name FROM pragma_table_info('usage_counter') ORDER BY cid")?;
    let names = st
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::{bump, read_all, set_one};
    use rc_store::Root;

    /// `set_one` 与 `bump` 的**唯一**区别就在 SQL 那一行：`count = excluded.count`（覆盖）对
    /// `count = count + excluded.count`（累加）。这不是文档里的一句承诺，是两条路径同时挂出去、
    /// 读回来分得开的那一格——把 `excluded.count` 抄成 `count + excluded.count` 时本用例立刻红。
    ///
    /// 选 `maintenance:last_full_scan_round` 当靶：它就是 `set_one` 文档里点名的用例（全量扫描
    /// 触发时"置为本轮轮号"而不是"加一"），而 `bump` 那一格挑的是 `maintenance:rounds`，
    /// 两格主语同为空串、metric 不同 ⇒ 同一张表上并存也各自读得回来。
    #[test]
    fn set_one_覆盖不是累加() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        // 连写两次不同值，读回来必须是第二个值；写成累加的话这里是 7 + 9 = 16。
        set_one(root.ledger(), "maintenance", "", "last_full_scan_round", 7, 100).unwrap();
        set_one(root.ledger(), "maintenance", "", "last_full_scan_round", 9, 110).unwrap();
        let rows = read_all(root.ledger()).unwrap();
        assert_eq!(rows.len(), 1, "两写必须合到同一行（同一主语 + 同一 metric）：{rows:?}");
        assert_eq!(rows[0].count, 9,
            "set_one 是覆盖不是累加——累加的话这一格该是 16：{rows:?}");
        assert_eq!(rows[0].last_at_ms, 110,
            "覆盖那一写的时间戳也得跟着落（`last_at_ms = excluded.last_at_ms`）：{rows:?}");
        // 共证人：同一张表上 `bump` 那一格照常是累加。两条路径的分歧必须当场读得出来，
        // 否则"覆盖"与"累加"就只差一句注释，谁改了 SQL 都可能长期照绿。
        bump(root.ledger(), "maintenance", "", "rounds", 7, 100).unwrap();
        bump(root.ledger(), "maintenance", "", "rounds", 9, 110).unwrap();
        let rounds = read_all(root.ledger()).unwrap().into_iter()
            .find(|r| r.metric == "rounds")
            .expect("bump 写的那一行不见了");
        assert_eq!(rounds.count, 16, "bump 那一格是累加 7 + 9：{rounds:?}");
    }
}
