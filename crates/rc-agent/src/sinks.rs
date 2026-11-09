use crate::error::ToolError;

/// **写路径的唯一出口，由调用方实现。** 它存在的原因是守卫 6 的后门：`rc-agent` 一旦直接依赖
/// `rc-ledger` 或会话句柄，`hook → rc-agent` 就把转录层带进了节点侧读路径。缝留在这一侧，
/// 实现留在那一侧，`hook` 因此**拿不到 `TurnSink` 的任何合法实现**——这就是"agent 的写入必须
/// 走中心"在类型层的形状，不需要一条纪律来维持。
///
/// 只有一个方法，而且它**不接 `role` 形参**（任务 8 开头 (2)）：§8 的 `strong_anchor` 判的是
/// "锚解析到人工轮"，role 若可由调用方填，那个开关就变成调用方可写的字符串。
/// 阶段 2 要写人工轮时加的是 `append_human_turn` 这**第二个方法**，不是把参数补回来。
pub trait TurnSink {
    /// 返回新轮的 `turn_uid`（64 hex）。`Err` 只许带 `ToolError`：缝的实现方不许把
    /// SQL 错误或文件路径顺着这条线递上来（守卫 12 的零路径约束在接口层就先立住）。
    fn append_agent_turn(
        &self,
        session_key: &str,
        body: &str,
        now_ms: i64,
    ) -> Result<String, ToolError>;
}

/// P2 的三个读数（§6.5 那一行的原样兑现）。**没有 `body`，也没有 `title`**（P-46）：
/// 缝是 `rc-agent` 唯一能拿到水位库数据的通道，它一旦能装正文，"零正文"就只剩 `surface.rs`
/// 一句口头承诺，而 `ToolJson` 里会多出一条可自由拼接的字符串。
///
/// 三个字段全是 `pub`：字段名**就是**回执里的三个键名（见 `fields()`），私有字段会把
/// P-46① 那条"数格数"的用例挤成"数 Rust 字段"，测不到 JSON 面。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TurnExtraction {
    /// `rc_ledger::water::ExtractState::slug()` 的原样搬运。类型是 `String` 不是枚举：
    /// 本 crate 的依赖清单里没有 `rc-ledger`（那正是缝存在的理由），所以这不是扩权是形状。
    pub extract_state: String,
    /// 拒收/无价值的理由短语，**透传**（P-46②）。封闭词表的家在 `rc_distill::REASON_SLUGS`，
    /// 而 `rc-agent` 与 `mcp` 都不依赖 `rc-distill` ⇒ 这两面不许抄那份词表来断言。
    pub reason: Option<String>,
    /// **队列位置**，不是时间承诺（P-44）：该会话游标与这一轮之间的转录行数，左开右开。
    pub eta_turns: u64,
}

impl TurnExtraction {
    /// 三个键名的**唯一家**：`remember` 把它们摊平在顶层（§6.5 的原样承诺），`status(turn=)`
    /// 把它们挂在 `"extraction"` 那一格下面。两处键名因此不可能漂，而
    /// `mcp/tests/p2_extract_state.rs` 的逐字段对撞正是这条的读者。
    /// 与 `AgentErrorKind::slug()` 同一对 `expect` 形状（派生序列化不会失败，unwrap 是掩盖不是简化）。
    pub fn fields(&self) -> serde_json::Map<String, serde_json::Value> {
        serde_json::to_value(self)
            .expect("TurnExtraction has derived Serialize over scalar fields")
            .as_object()
            .expect("a struct serializes to a JSON object")
            .clone()
    }
}

/// **读缝**：`rc-agent` 拿水位库数据的唯一通道，与 `TurnSink` 同构。
/// 它必须是一条缝而不是直接依赖——本 crate 的依赖清单里没有 `rc-ledger`、也没有 `rusqlite`
/// （那份 Cargo.toml 的注释就是守卫 6 后门的原文）。有了它，`hook` 在类型上就拿不到水位库：
/// `AgentRoot` 只有记忆柄，而缝的实现方需要一个 `Root`。任务 13 往这个 trait 加第二条方法
/// （`cycle_health`），**不加第二个 trait**：一面一缝，两缝就是下一次漂移的地方。
///
/// 两条从 `TurnSink` 继承的纪律：
/// - `Err` 只许带 `ToolError`：实现方不许把 SQL 错误或路径顺着这条线递上来（零路径在接口层先立住）。
/// - `Ok(None)` 是**读数**，不是错误；而**分型不在缝里**（P-62：`status` 把它读成 `Empty`、
///   `remember` 把它读成 `Unavailable`，两个映射住 `rc-agent`，见 `extraction_readout` 的注释）。
///
/// P5/P6 的载荷。裸读数与裁定逐格分开（§2.20 禁令 2 / S3a）：
/// `lag_turns` 是"转录总行数 − `extract_pass_state` 里有终态行的轮数"（S3b 起换源：legacy
/// `extract_state` 今天没有写者，daily 路径写的是 pass 表），`last_success_at_ms` 是终态行
/// `updated_at_ms` 的最大值，`llm_calls_used` 是 `usage_counter` 里 `llm_calls` 那一支的总和。
/// 这个 `llm_calls_*` 数字**不参与"额度耗尽"那一格的判定**：S3a 起那一格搬的是
/// `quota_stopped`（生产者 = 每次运行收尾的 `set_one`）。此后成本读数由 `llm_calls_total`
/// 一个字段承担（同一份 `usage_counter` 求和）；`llm_calls_used` 因此**今天零读者**——它
/// 是判定曾经的输入，按 C2 保留字段、不删、登记为 dormant（见 spec §4 的 dormant 脚注）。
/// 它**不是任何一格的判定输入**，也不许被重新当成判定输入：额度那一格只读 `quota_stopped`。
///
/// 与它同族的那一格"单轮上限"2026-11-06 随"调用配额是预算，砍掉"整格删除——上限本身
/// 不再存在，留着就是一条没有生产者的常数。
///
/// `last_success_at_ms` 为什么是 `Option` 而 `lag_turns` 不是：前者在库里今天就可以一行都没有
/// （从没蒸完过一轮），后者是两行数相减（转录总数 − 终态轮数）、`0` 是一个真读数。两者的形状
/// 都由数据层的事实决定，不是为了 `status` 那一格好看。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CycleHealth {
    pub lag_turns: i64,
    pub last_success_at_ms: Option<i64>,
    /// C2 / dormant：`usage_counter.llm_calls` 的求和，判定曾经的输入。
    /// 累计成本读数走 `llm_calls_total`（`Option` 形状，能说"账上还没有这一行"），
    /// 于是这一格**零读者**；保留字段、不删，不许拿它当判定输入。
    pub llm_calls_used: i64,
    /// P6 的时间格（裁定 20：缺席 ⇒ `null`，P-54）。
    pub last_derive_at_ms: Option<i64>,
    /// **这一格是读数的输入，不是读数**：`derive` 水位行的游标两列，`None` = 那行还不存在
    /// （= 一行都还没推导过，是一个真读数，不是"读不到"）。
    ///
    /// 为什么由缝搬而不是由消费面自己读：`count_rows_since` 的谓词要 `(ms, key)`，这两列住在
    /// 水位库，而 `rc-agent` 依守卫 6 拿不到 `LedgerHandle`、`mcp` 依判据 6e/5 够不到
    /// `rc-memory`。两难之下唯一不新增依赖边的形状就是把**位置**当输入搬过去，行数在拿到的
    /// 那一侧现算。别把它读成"缝多带了一个中间量"：`status` 里那一行 `count_rows_since(..)`
    /// 是这一格唯一的生产者，缝只负责把游标送到生产者手边。
    ///
    /// `Copy` 因这个 `String` 而去掉。全仓被这一笔波及的调用点**只有一处**：
    /// `tools_shape.rs` 的 `Tap::cycle_health` 从 `Mutex` 的借用里 `.map(Ok)`，去掉 `Copy`
    /// 后要先 `.clone()`（Step 5 末尾的改动面清单里第 4 项就是它）。
    pub derive_cursor: Option<(i64, String)>,
    /// `anomaly_scan` 水位行的 `last_success_at_ms`——心跳，不是错误码。
    /// 走缝而不直读：生产者 `water::last_extract_at_ms` 要 `LedgerHandle`，守卫 6 不许 rc-agent 拿它。
    pub last_anomaly_scan_at_ms: Option<i64>,
    /// S3a：**最近一次记过账的运行**是否因额度耗尽而停。`None` = 还没有运行记录
    /// （与 `false` 是两件事：后者是"跑过且没触顶"）。"记过账"的集合 = naming / fact / event /
    /// state / cycle 五个收尾口（`cycle` 不是 pass，`time` 这个 pass 没有模型面），所以这一格说的是
    /// "集合内最近一次收尾留下的值"，不是"全仓最后一次进程退出"。这一格是
    /// `status` 里"额度耗尽"那一格的**唯一**来源；`llm_calls_used` 从 S3a 起已 dormant
    /// （零读者，见结构体文档），不是成本读数、更不是判定输入（88 次生命周期累计 ≠ 本轮触顶）。
    pub quota_stopped: Option<bool>,
    /// S3a/R3：生命周期累计 `llm_calls`（成本读数）。`None` = 账上一行 `llm_calls` 都没有
    /// （`0` 会被读成"跑过且一次没花"）。与 `llm_calls_used` 同值：那格已 dormant（零读者），
    /// 这一格是 `status.llm_calls_total` 的唯一搬运道。
    pub llm_calls_total: Option<i64>,
}

/// G19 刀 4 的会话两阶段读数（`recollect_ingest_status` 的缝载荷）。裸读数：
/// `pending` 的减法住 `tools::ingest_status`（分型与键名在工具层，缝只搬运——与
/// `cycle_health` 搬 `derive_cursor`、行数由消费面现算的分工同一条）。
///
/// 两个口径都写死在实现方（mcp 的 `ingest_counts`），这里逐格声明：
/// `turns_total` = **可抽取轮**（`role <> 'system'`）——system 轮（工具调用/结果、压缩块）
/// 只作上下文、永不写终态 pass 行，把它数进分母就是 `count_extractable_turns`
/// 那条注释点名的"恒清不掉的假积压"；`last_arrival_ms` = 该会话**全部**轮的
/// `max(ingested_at_ms)`（到达钟，与选批同一根轴；`None` = 一条都没到，
/// "没有"不是 0——P-54 同律）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestCounts {
    pub turns_total: i64,
    pub distilled: i64,
    pub last_arrival_ms: Option<i64>,
}

pub trait CycleRead {
    fn turn_extraction(&self, turn_uid: &str) -> Result<Option<TurnExtraction>, ToolError>;
    /// 整库一读数，**不收参数**：P5 那四格都是跨 scope 的总和/最值（架构 §9.5 ① 那格的口径），
    /// P6 追加的两格是推导家族跨 scope 的最值与批末位置（各字段自己的注释说清它们是什么）。
    /// 实现方报错 ⇒ `status` 整个报错，不折成 `null`（P-61 的镜像面：`status` 没有
    /// "已经写下去了"这个既成事实要保护，它的成功与否**就是**"读数拿没拿到"）。
    fn cycle_health(&self) -> Result<CycleHealth, ToolError>;
    /// 全库停轮 scope 数（A1）。与 cycle_health 不同——这条**在 turn/entity 路径上也读**，
    /// 实现方不可短路。缺席 ⇒ `status` 顶层的两格之一为 `null`（P-54 同族）。
    fn halted_scopes(&self) -> Result<i64, ToolError>;
    /// 每 scope 的 lag，按降序。**`Vec<(String, i64)>` 而非 `Vec<EntityEntry>`**：
    /// 渲染语义住 `tools.rs` 的 `status`，缝只给裸数据（与 `cycle_health` 同族）。
    fn lag_scopes(&self) -> Result<Vec<(String, i64)>, ToolError>;
    /// P0-1：**有效章**覆盖的会话数。它读的是转录库（`session_principal_claim`），
    /// 而 `rc-agent` 手里只有记忆柄 ⇒ 只能经缝搬。
    ///
    /// 为什么带默认实现（本 trait 唯一一条）：这一格与 `cycle_health` 那四格不是同一份账，
    /// 把"缝没实现它"折成 `store_unreachable` 会让所有只关心其它读数的装配面凭空失败；
    /// 默认回 `None` ⇒ `status` 渲染 `null`（键恒在场、值可为 null，P-54 同一条）。
    /// 实现方**真的读失败**仍要报错（`Err`），不许把"读不到"折成 `None`。
    fn principal_claims(&self) -> Result<Option<i64>, ToolError> {
        Ok(None)
    }
    /// G19 刀 4：一个会话的导入/可查两阶段读数。`Ok(None)` = **会话不在**（查无，读数），
    /// `Err` = **这一面答不了**。两种"没有"不许混成一格（P-54 的缝面形状），所以缺省实现
    /// 响亮拒绝而不是回 `None`：`ingest_status` 是 mcp 专属工具（导入发生在中心面），
    /// `hook` 一侧的缝实现不接这一格，缺省拒绝让"实现方忘了接"与"会话真的不在"
    /// 从分型上就分得开。实现方**真的读失败**同样要报 `Err`，不许折成 `None`。
    fn ingest_counts(&self, session_key: &str) -> Result<Option<IngestCounts>, ToolError> {
        let _ = session_key;
        Err(ToolError::unavailable(
            "extraction_not_readable",
            "this face has no transcript access; ask the center",
        ))
    }
}
