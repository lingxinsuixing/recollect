//! B2b-2：节点离线自蒸——**一个有界批次**，写暂存层，留行标。
//!
//! 边界（派发档 §一）：触发接线（1 天闸、`observe_failure` → `offline_distilling`）、
//! `status`/`doctor` 的读数、回并都在别的批次。这里只回答三件事：
//!
//! 1. 本地能蒸出来：`rc_cycle::run_cycle_once` 跑在**暂存层**（`pending.db`）上，
//!    转录与水位仍是节点自己的两册；
//! 2. 写的是暂存层：底座副本（`replica/snapshot-*.db`）一个字节都不动；
//! 3. 有行标：本批新写下的行按 `staging_new_row` 的键形记进标记表（B1b 的读面据此加
//!    `pending: true`），回并在 B3。
//!
//! **已蒸过的 turn 不重蒸**：暂存层是底座副本的拷贝，它的 `provenance` 就是「中心那边已经
//! 蒸过的 turn 清单」。这些 turn 在节点账本里可能还没有 `extract_state` 行（账本不出中心），
//! 于是 `select_batch` 会把它们当新 turn 选进来——本模块在跑 cycle 之前按 `provenance` 播种
//! `Extracted`，这就是工单 §二.7 那句「只取尚未出现在副本 provenance 的 turn」的落点。
//! **不新造采集器、不改 `select_batch`**。

use crate::node_cfg;
use crate::staging;
use crate::state::{SelfDistill, SyncState};
use rc_cycle::{CycleExit, Metered};
use rc_ledger::water::{self, ExtractState};
use rc_llm::{LlmBackend, LlmFailure, Prompt, Rotating};
use rc_store::Root;
use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

/// 离线自蒸的封闭失败面（派发档 §二.2）。一个变体一条去向；`Cycle` 的载荷只许是
/// `rc_cycle` 的类别串（守卫 12：运维面没有正文、没有路径）。
#[derive(Debug, PartialEq, Eq)]
pub enum OfflineFault {
    /// `auto_unlock = false`：节点被配置为不自动开箱。
    Locked,
    /// `node.toml` 没给 `llm_config`（或指到的那份 `llm.toml` 不在）。
    NoConfig,
    /// `llm.toml` 配坏了 / `secret_ref` 不合法 / preset 表里查不到那一对。
    BadConfig,
    /// 密文箱还没建。
    BoxMissing,
    /// 箱在，口令文件不在。
    PassMissing,
    /// 箱与口令都在，但打不开（口令错、文件坏）。
    BoxOpen,
    /// 箱能开，里面没有 `secret_ref` 点名的那一格。
    SlotMissing,
    /// 暂存层不在：`pending.db` 由 B1b 的 `staging::prepare` 建，B2b-3 负责在 1 天闸之后调它。
    StagingMissing,
    /// `capability::probe` / `rc_cycle::qualify` 不过：这台机器的三库不适配抽取。
    Unqualified,
    /// `rc_cycle::CycleExit::Fault` 的原样类别串。
    Cycle(&'static str),
}

/// 一个有界批次的读数：管线那十七格 + 本批新写的标记行数。
///
/// `staging_rows` 是**本批**差集的大小（`after − before`），不是暂存层的总行数——
/// 后者由读面直接查标记表（B2b-3 的 `staging_rows` 读数），这两个数不是一件事。
pub struct OfflineRun {
    pub out: rc_cycle::CycleOut,
    pub staging_rows: usize,
}

/// 对已存在的暂存层跑**一个有界批次**。后端由调用方给（生产是 Live 轮转，测试是脚本）。
///
/// 顺序是派发档 §二.2 定死的九步；`now_ms` 由调用方递进来（判据 12a：库代码不读钟）。
/// **索引不在这里重建**：`run_cycle_once` 收尾已经调过 `rc_index::rebuild`（守卫 3）。
pub fn run_offline_cycle(
    data_dir: &Path,
    backend: &dyn LlmBackend,
    meter: &Metered,
    now_ms: i64,
) -> Result<OfflineRun, OfflineFault> {
    // 1. 暂存层存在性是**前置条件**，不是错误兜底：没有它就没有可写的记忆库，
    //    也就没有"本地能蒸出来"这件事。响亮失败（B2b-3 据此决定要不要先 prepare）。
    if !staging::is_active(data_dir) {
        return Err(OfflineFault::StagingMissing);
    }
    // 2. 三库装配：记忆库换成暂存层，转录与水位仍是节点自己的。
    //    打不开库没有独立变体；按 `rc-cycle` 的封闭类别串折叠（同一个 doctor 去向），
    //    不新造一个只为一行代码存在的变体。
    let root = Root::open_with_memory(data_dir, &staging::path(data_dir))
        .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_READ))?;
    // 3. 资格只在入口判一次（守卫 14）：抽不出资格就不许烧配额。
    let cap = rc_store::capability::probe(root.transcript(), root.ledger(), root.memory())
        .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_READ))?;
    if !rc_cycle::qualify(&cap) {
        return Err(OfflineFault::Unqualified);
    }
    // 4. 已蒸过的 turn：副本 provenance 里的每一个 uid 都要在节点账本里有状态行，
    //    否则 `select_batch` 会把底座里已有的 turn 重蒸一遍（既烧配额，又在新行里重复）。
    seed_already_distilled(&root, now_ms)?;
    // 5. 基底键集：`after − before` 就是本批的新行。**必须在 cycle 之前读**。
    let before = key_set(root.memory())
        .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_READ))?;
    // 6. 一轮 = 一个批次（`rc_cycle` 的既有语义，与中心逐字同一条管线）。
    let (out, exit) = rc_cycle::run_cycle_once(&root, meter, backend, now_ms,
                                               rc_cycle::DEFAULT_BATCH_TURNS);
    // 7. 调用次数进节点自己的账本。先记账再判 fault：`CycleExit::Fault` 之前发生过的那几次
    //    调用是**真发生的**（P-33 的算术），不许因为这一轮以失败收场就不认账。
    rc_cycle::charge_counters(root.ledger(), meter, now_ms)
        .map_err(OfflineFault::Cycle)?;
    // 8. 差集入标记。失败轮次也照做：已经落进暂存层的行仍然是"离线期新行"，
    //    下一轮重放靠 `ON CONFLICT DO NOTHING` 幂等，标记表不跟着写就会漏标。
    let after = key_set(root.memory())
        .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_READ))?;
    let staging_rows = mark_new_rows(root.memory(), &before, &after)
        .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_WRITE))?;
    // 9. 判定与读数是两件事：`Fault` 原样上交类别串，`Ran` / `BatchDropped` 都算跑过
    //    （后者是契约丢弃，批次里一行都没写，游标不推，下一轮重来——不是运维故障）。
    match exit {
        CycleExit::Fault(cat) => Err(OfflineFault::Cycle(cat)),
        CycleExit::Ran | CycleExit::BatchDropped => Ok(OfflineRun { out, staging_rows }),
    }
}

/// 触发点（B2b-3）：`self_distill == OfflineDistilling` 才动手——先 `staging::prepare`
/// （幂等：层已在就原样读回），再 `run_offline_cycle` 跑**一个有界批次**。
///
/// 未到 `OfflineDistilling`（窗口没满，或节点被锁在 `auto_unlock=false`）⇒ `OfflineFault::Locked`：
/// **不建层、不烧配额**。两种原因的处置是同一件"这一轮什么都不做"，不新造第二个变体。
/// `prepare` 失败（底座副本不在/拷不动）⇒ `StagingMissing`：层没建出来就是那条故障本身。
pub fn run_one_offline_batch(
    data_dir: &Path,
    st: &SyncState,
    backend: &dyn LlmBackend,
    meter: &Metered,
    now_ms: i64,
) -> Result<OfflineRun, OfflineFault> {
    if st.self_distill != SelfDistill::OfflineDistilling {
        return Err(OfflineFault::Locked);
    }
    staging::prepare(data_dir, &st.current_manifest, now_ms)
        .map_err(|_| OfflineFault::StagingMissing)?;
    run_offline_cycle(data_dir, backend, meter, now_ms)
}

/// 第 4 步：把副本 `provenance` 里的 turn 在节点账本里标成 `Extracted`。
///
/// 只补**没有状态行**的：库里已有 `Rejected` / `Worthless` / `Pending` 的那些是节点自己
/// 蒸出来的判断，不许被这一层覆盖。`DISTINCT` 在一张只读的集合查询里做完，
/// 循环里只剩"查一次、缺就写一次"（不新造采集器）。
fn seed_already_distilled(root: &Root, now_ms: i64) -> Result<(), OfflineFault> {
    let uids: Vec<String> = {
        let mut st = root
            .memory()
            .conn()
            .prepare("SELECT DISTINCT turn_uid FROM provenance")
            .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_READ))?;
        let got = st
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_READ))?;
        got.collect::<Result<Vec<_>, _>>()
            .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_READ))?
    };
    for uid in uids {
        let known = water::get_extract_state(root.ledger(), &uid)
            .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_READ))?;
        if known.is_none() {
            water::set_extract_state(root.ledger(), &uid, ExtractState::Extracted, None, now_ms)
                .map_err(|_| OfflineFault::Cycle(rc_cycle::FAULT_STORE_WRITE))?;
        }
    }
    Ok(())
}

/// 基底键集 / 本批差集读的那张表。`(table_name, 取键 SQL 表达式)`。
///
/// 事实族取**句柄形**（`<table>:<id>`，即 `MemKey::key_string()`）而不是裸 `id` 列：
/// B1b 的读面拿回执里的 `id` 与标记表逐字比对，回执里的 `id` 就是句柄（`rc-agent` 的
/// recall 与 `mcp` 的 pending 行标都是同一个串）；裸 `id` 永远匹配不上。复合键用派发档
/// §二.2 表格里的拼接，分隔符就是 `:`，不做二次转义。`fact_link` 不在读面的行集里
/// （边不是可指认句柄），按表格原样取裸 `id`。
///
/// 表集是 canonical 词表（25 表）：旧的 `card`/`event`/`assertion` 已合并为单 `card` 表，
/// `entity`→`anchor`，`entity_alias`→`alias`，`mention`→`memory_mention`，
/// `relationship`/`event_link`→`fact_link`。旧名在 canonical 库上不存在，读它们会直接
/// SQL 失败（`store read failed`）。
///
/// **这里是键表达式的唯一一份**：`key_set` 拼 `SELECT <expr> FROM <table>`，
/// B3b 的 `reconcile` 把同一个 `<expr>` 放进 `JOIN staging_new_row` 的 ON 里。
pub(crate) const KEY_SQL: [(&str, &str); 9] = [
    ("card", "'card:' || id"),
    ("anchor", "'anchor:' || id"),
    ("fact_link", "id"),
    ("attestation", "'attestation:' || id"),
    ("attention", "'attention:' || id"),
    ("provenance", "mem_table || ':' || mem_id || ':' || turn_uid"),
    ("alias", "anchor_id || ':' || alias"),
    ("memory_mention", "mem_table || ':' || mem_id || ':' || anchor_id"),
    ("retirement", "mem_table || ':' || mem_id"),
];

/// 全库键集。9 张表各一次全表读：一期的库量下无所谓，而"差集"要的是**两张快照**，
/// 不是"本批写了什么"（管线里没有一个函数会告诉我们它写了哪些表）。
fn key_set(m: &rc_store::MemoryHandle) -> Result<HashSet<(String, String)>, rc_store::StoreError> {
    let mut out = HashSet::new();
    for (table, expr) in KEY_SQL {
        let mut st = m.conn().prepare(&format!("SELECT {expr} FROM {table}"))?;
        let rows = st.query_map([], |r| r.get::<_, String>(0))?;
        for row in rows {
            out.insert((table.to_string(), row?));
        }
    }
    Ok(out)
}

/// 第 8 步：`after − before` 写进 `staging_new_row`（`INSERT OR REPLACE`：同一键重复标记幂等）。
/// 返回本批新写的行数。
fn mark_new_rows(
    m: &rc_store::MemoryHandle,
    before: &HashSet<(String, String)>,
    after: &HashSet<(String, String)>,
) -> Result<usize, rc_store::StoreError> {
    let fresh: Vec<&(String, String)> = after.difference(before).collect();
    if fresh.is_empty() {
        return Ok(0);
    }
    let tx = m.conn().unchecked_transaction()?;
    {
        let mut st = tx.prepare(
            "INSERT OR REPLACE INTO staging_new_row(id, table_name) VALUES(?1, ?2)",
        )?;
        for (table, id) in &fresh {
            st.execute([id.as_str(), table.as_str()])?;
        }
    }
    tx.commit()?;
    Ok(fresh.len())
}

/// status 与状态文件的钥匙面读数（B2b-3）。三值封闭、零路径零值：
///
/// - `locked`：`auto_unlock=false`——节点被配置为不自动开箱（这一条比"钥匙在哪"更靠前）；
/// - `unlocked`：配置与每一格都读得到（生产装配走得通）；
/// - `fault`：配置缺、坏，箱与格有问题（`build_live_backend` 的其余每一条故障）。
///
/// 判定**复用 `build_live_backend`**：要回答的本来就是"装得起来吗"这同一个问题，
/// 不在这里写第二套箱/格检查（守卫 12：同一件事不许有第二个家）。
pub fn live_key_reading(cfg: &node_cfg::NodeConfig, data_dir: &Path) -> &'static str {
    // 一次性的计数器，只为把装配结果分类；不落任何账（读数是"能不能开箱"，不是"开过几次"）。
    let meter = Rc::new(Metered::new());
    match build_live_backend(cfg, data_dir, meter) {
        Ok(_) => "unlocked",
        Err(OfflineFault::Locked) => "locked",
        Err(_) => "fault",
    }
}

/// 单发墙（毫秒）：节点每一次出网调用的上限。
///
/// 与 hub 同源（W19c1 起两边都取 [`rc_llm_http::transport::CALL_WALL_MS`]）：一个永不
/// 回复的对端在 300 秒处被切断，交回 `LlmFailure::Timeout`，重试/隔离（3 次、
/// `retry_exhausted`）才有机会跑。
///
/// **周期预算不再是出网超时**：`rc_llm::cycle_budget_ms()`（四族之和，约 50 分钟）是
/// "一轮最多烧多少"的账，不是"这一发最多挂多久"的墙；把它当单发超时会让节点比 hub
/// 多挂近 10 倍（用户 2026-11-06 裁定：预算已砍，只留卡死墙）。那一格本身保留，
/// 别处仍在用。
fn seat_deadline_ms() -> u64 {
    rc_llm_http::transport::CALL_WALL_MS
}

/// 生产装配（派发档 §二.3）：`auto_unlock` / `llm.toml` / 密文箱 / 格，四种故障各自一个变体；
/// 每座一个 `Live`，外面包一层 [`MeteredBackend`]，装进 `Rotating`。
///
/// **不读也不写 `llm_state`**：B2b-3 的触发接线在同一个节点进程里做，本批只证明
/// "装得起来、调用会记账"。**不碰网络**：`Live` 的构造点只查 preset 表与参数合法域，
/// 出网点（`transport::agent`）在构造时一个请求都不发。
pub fn build_live_backend(
    cfg: &node_cfg::NodeConfig,
    data_dir: &Path,
    meter: Rc<Metered>,
) -> Result<Rotating, OfflineFault> {
    // 顺序是有牙齿的：`auto_unlock=false` 是"节点被配置为不自动开箱"，
    // 它比"钥匙在哪"更靠前——锁定态不该被读成配置事故。
    if !cfg.auto_unlock {
        return Err(OfflineFault::Locked);
    }
    let Some(path) = cfg.llm_config.as_deref() else {
        return Err(OfflineFault::NoConfig);
    };
    // `Ok(None)` = 指到的那份 `llm.toml` 不在（"这台机器还没配"），与坏文件是两种后果；
    // 两者都不是"锁着"，但"没有配置"比"配置坏了"更靠前——它的修法是补文件。
    let llm_cfg = match rc_llm_cfg::load(path) {
        Ok(Some(c)) => c,
        Ok(None) => return Err(OfflineFault::NoConfig),
        Err(_) => return Err(OfflineFault::BadConfig),
    };
    let deadline_ms = seat_deadline_ms();
    let mut slots: Vec<(String, Box<dyn LlmBackend>)> = Vec::with_capacity(llm_cfg.seats.len());
    for (i, seat) in llm_cfg.seats.iter().enumerate() {
        let key = crate::llm_key::read_llm_key(data_dir, &seat.secret_ref)
            .map_err(key_fault)?;
        let live = build_live(seat, deadline_ms, key.as_str().to_owned())?;
        // 槽位名是**路名**（不进 stderr、不进状态文件）：`seat-1` 与 hub 的 `reply-N` 同一形状。
        slots.push((
            format!("seat-{}", i + 1),
            Box::new(MeteredBackend { inner: live, meter: Rc::clone(&meter) }),
        ));
    }
    // `llm.toml` 的 `load` 已经拒了空列表；这一支是 `Rotating::new` 自己的形状校验，
    // 落在这里只剩"座位数 0"的一种可能，它不是配置事故之外的任何东西。
    Rotating::new(slots).map_err(|_| OfflineFault::BadConfig)
}

/// 读箱失败 → 离线故障。`BadRef` / `BadBox` 是"值的位置不合法"（配置事故），
/// 其余四类各归一条读箱事故——与 `llm_key` 的封闭枚举同形，不给两个归并口。
fn key_fault(e: crate::llm_key::LlmKeyFault) -> OfflineFault {
    use crate::llm_key::LlmKeyFault as E;
    match e {
        E::BadRef | E::BadBox => OfflineFault::BadConfig,
        E::BoxMissing => OfflineFault::BoxMissing,
        E::PassMissing => OfflineFault::PassMissing,
        E::BoxOpen => OfflineFault::BoxOpen,
        E::SlotMissing => OfflineFault::SlotMissing,
    }
}

/// 一座 `Live`。两条通道各一个构造器，preset 查表与参数合法域校验都在构造点
/// （`rc_llm_http` 的接缝：查不到键就 `Err`，不回落默认）。
fn build_live(
    seat: &rc_llm_cfg::Seat,
    deadline_ms: u64,
    key: String,
) -> Result<rc_llm_http::Live, OfflineFault> {
    let egress: Box<dyn rc_llm_http::Egress> = Box::new(rc_llm_http::transport::agent(deadline_ms));
    match (&seat.provider, &seat.endpoint, seat.model.as_deref()) {
        (Some(p), None, Some(m)) => rc_llm_http::Live::new(
            p, m, key, rc_llm_http::MAX_BUDGET, None, egress),
        (None, Some(ep), Some(m)) => rc_llm_http::Live::new_custom(
            &ep.base_url, &ep.protocol, m, key, rc_llm_http::MAX_BUDGET, None, egress),
        // `rc_llm_cfg::load` 已保证"两条通道恰有一条、custom 侧必有 model"；
        // 落到这里只剩 preset 侧缺 `model` 的一种形状——对 preset 表就是查不到键。
        _ => Err("seat has no resolvable preset key"),
    }
    .map_err(|_| OfflineFault::BadConfig)
}

/// 座位 + 账本：调完内层**无论成败**都落一次账（`record_call`）——"开过口"与"答得好"
/// 是两件事（P-33 的算术）。本层**不设**本地调用上限：一轮问几次由 `rc-distill` 的批大小定。
///
/// `QuotaExhausted` 是唯一要多做一件事的结果：`Rotating` 不持任何调用账，所以那条
/// "后端自报额度耗尽"的记录只能在这里进 `CycleOut.quota_exhausted`；其余失败态原样上抛。
struct MeteredBackend {
    inner: rc_llm_http::Live,
    meter: Rc<Metered>,
}

impl LlmBackend for MeteredBackend {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        let got = self.inner.complete(prompt);
        self.meter.record_call();
        if matches!(got, Err(LlmFailure::QuotaExhausted)) {
            self.meter.mark_quota_exhausted();
        }
        got
    }
}

#[cfg(test)]
mod tests {
    use super::seat_deadline_ms;

    /// W19d：节点每次出网的超时是**单发墙**，与 hub 同一个常数；并且严格小于
    /// 周期预算——后者是"一轮最多烧多少"的账，不是任何一次调用的墙。
    ///
    /// 变异判据：把 `seat_deadline_ms` 的实现改回 `rc_llm::cycle_budget_ms()`，
    /// 第二条断言必红（3_000_000 < 3_000_000 不成立）。它排在最前面，就是为了让
    /// 变异先撞上这一条，留下"预算不是出网超时"的直接证据。
    #[test]
    fn seat_deadline_is_the_single_call_wall_not_the_cycle_budget() {
        assert!(
            seat_deadline_ms() < rc_llm::cycle_budget_ms().max(0) as u64,
            "单发墙必须严格小于周期预算：预算不是出网超时"
        );
        assert_eq!(
            seat_deadline_ms(),
            rc_llm_http::transport::CALL_WALL_MS,
            "节点出网超时必须与 hub 同源（单发墙）"
        );
    }
}
