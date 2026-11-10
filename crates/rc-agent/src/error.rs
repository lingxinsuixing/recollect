use serde::Serialize;

/// 运维面四类分型。**不许合并的判据是"人下一步做什么不同"**（§10 权限④）：
/// 改输入 / 找中心或改配置 / 等一次重建 / 承认库还没东西——四条动作两两不同，所以四型必须都活着。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentErrorKind {
    /// 输入本身不合法（空查询、坏句柄形状、代词别名）。
    Validation,
    /// 这份句柄/身份没有做这件事的资格（只读副本要求写、越界 actor）。
    Authorization,
    /// 有能力但此刻不行（索引未建、尺子版本不符、阶段 4 的中心不可达与未配对落这里）。
    Unavailable,
    /// 服务正常、输入正常、库里没有。
    Empty,
}

impl AgentErrorKind {
    pub const ALL: [AgentErrorKind; 4] = [
        AgentErrorKind::Validation,
        AgentErrorKind::Authorization,
        AgentErrorKind::Unavailable,
        AgentErrorKind::Empty,
    ];
    /// 与 `RejectCode::slug()` / `GateCell::slug()` 同一个理由：渲染处只许一个。
    pub fn slug(self) -> String {
        serde_json::to_value(self)
            .expect("closed enum with derived Serialize")
            .as_str()
            .expect("serializes to a string")
            .to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    pub kind: AgentErrorKind,
    /// 稳定的机读码（阶段 4 后它是错误目录的键，所以它必须是 `&'static str` 不是格式化串）。
    pub code: &'static str,
    /// 人该敲的下一条命令或该改的那个输入。**零正文、零输入回显**（守卫 12 + 阶段 0 的 `safe_token` 教训：
    /// 键名与句柄都能长成路径，回显等于把用户字节送进 stdout）。
    pub hint: &'static str,
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} {}", self.kind.slug(), self.code, self.hint)
    }
}

impl ToolError {
    pub fn validation(code: &'static str, hint: &'static str) -> Self {
        ToolError { kind: AgentErrorKind::Validation, code, hint }
    }
    pub fn authorization(code: &'static str, hint: &'static str) -> Self {
        ToolError { kind: AgentErrorKind::Authorization, code, hint }
    }
    pub fn unavailable(code: &'static str, hint: &'static str) -> Self {
        ToolError { kind: AgentErrorKind::Unavailable, code, hint }
    }
    pub fn empty(code: &'static str, hint: &'static str) -> Self {
        ToolError { kind: AgentErrorKind::Empty, code, hint }
    }
}

/// **分型映射只有一个家**：所有下层错误到这里，然后所有出口（mcp 的 JSON-RPC error、hook 的退出码、
/// doctor 的读数）都读 `kind`。这里写漏一支的默认行为必须是"响亮"的，所以下面三张表（`SearchError`、
/// `MemError`、`IndexError` 各一张）**逐变体点名、
/// 没有 `_` 兜底**：`SearchError` 的每一支、`IndexError` 的每一支、`StoreError` 的每一支都单独成行，
/// 新增变体当场 E0004（裁定 R40 把剩下的两处 `(_)` 通配也拆了，这句才第一次完全为真）。
/// `SearchError::Index(..)` 与 `From<IndexError>` 是**同一张真值表抄两遍**（R40 否掉了委托），
/// 相同由 `tests/tools_shape.rs::两个来源的分型映射不许漂` 执法，执法面是 **`kind`/`code`/`hint`
/// 三个维度、`From<IndexError>` 那八臂逐臂对撞**（每臂各构造一条同形状的载荷，两条路各自 `into()`
/// 一次再比三元组；工单 R50 之前这里只比 `(kind, code)` 两元组，hint 一维全无读者）。
/// 载荷不参与读数——八臂的 `kind`/`code`/`hint` 全是常量串，所以每臂一个载荷就够覆盖那一臂。
/// 这条用例执法的是下面那张表的**八臂**与 `SE::Index(..)` 各支之间那条接缝。两张表自己的
/// **Store 臂**（`SE::Store(..)` 那几行）与 `SE::Index(Store(..))` 之间**不要求同读数**，也不被对撞：
/// `SE::Store(ReadOnly)` 与 `SE::Index(IE::Store(ReadOnly))` 同码不同 hint，
/// `SE::Store(SchemaTooNew)` 与 `SE::Index(IE::Store(SchemaTooNew))` 连码都不同
/// （`schema_too_new` vs `store_unreachable`）。那是两条不同来源的接缝，不是漂移
/// （工单 Minor #6 已裁："同码 ⇒ 同 hint" 在本 crate 不是不变量）。
/// 唯一越过那条接缝的既有断言是 `tools_shape.rs` 里 `IE::Store(Io)` 对 `SE::Store(Io)` 的三元对撞
/// （裁定 R45 那条），它比的是"两条路进来的 store 故障同格同码同 hint"，与本表的收窄无关。
/// 唯一的两处收窄：`SE::Index(IndexError::Store(_))` 与 `SE::Store(...)` 的码相同是**读数结论**不是兜底——
/// 里面每个 StoreError 变体都在 `SE::Store` 那几行点过名；`SE::Memory(e)` 委托给 `From<MemError>`
/// 是"一个家"这条本身要求的（见那支旁的说明），不是通配。
impl From<rc_search::SearchError> for ToolError {
    fn from(e: rc_search::SearchError) -> Self {
        use rc_index::IndexError;
        use rc_search::SearchError as SE;
        match e {
            SE::Index(rc_index::IndexError::NotBuilt) => ToolError::unavailable(
                "index_not_built",
                "run: hub index rebuild --data-dir <dir>",
            ),
            SE::Index(IndexError::NormMismatch { .. }) => ToolError::unavailable(
                "index_norm_stale",
                "analyzer changed; run: hub index rebuild --data-dir <dir>",
            ),
            SE::Index(IndexError::NoQueryTokens) => ToolError::validation(
                "query_has_no_tokens",
                "the query normalizes to zero indexable tokens; send actual words",
            ),
            SE::Index(IndexError::ReadOnly(_)) => ToolError::authorization(
                "replica_read_only",
                "this handle is a replica; it cannot maintain its own index",
            ),
            SE::Index(IndexError::Store(rc_store::StoreError::ReadOnly(_))) => ToolError::authorization(
                "replica_read_only",
                "this handle is a replica; it cannot maintain its own index",
            ),
            // 裁定 R18 落地的第八支（上游事实，brief 的映射原文写于七支时代，缺它）：
            // "索引层在、但逐行读不出来"与"从没建过索引"是两件不同的事，**必须给不同串**——
            // 共用一条就是把"这一层坏了"播报成"你还没跑重建"（`crates/hub/src/main.rs:125` 同判据）。
            // 载荷为空是上游刻意的（step 期原串会回显用户字节），所以这里也只能是常量串。
            SE::Index(IndexError::LayerUnreadable) => ToolError::unavailable(
                "index_layer_unreadable",
                "run: doctor",
            ),
            // 裁定 R40（改判乙+，推翻评审员的"委托 `From<IndexError>`"那一票）：这三行**逐变体点名**，
            // 码与 hint 逐字取自 Step 6 那张 `From<IndexError>` 权威表——任务 8 落地那张表时
            // 两张表必须逐字相同，而"相同"由 `tests/tools_shape.rs::两个来源的分型映射不许漂` 执法。
            // 写成委托会让那条用例**结构上不可能失败**（本项目：一条从未红过的守卫等于没有守卫）；
            // 保留两份表 = 漂移有人在。
            SE::Index(IndexError::Sql(_)) => ToolError::unavailable(
                "index_failed",
                "run: hub index status --data-dir <dir>",
            ),
            SE::Index(IndexError::Store(_)) => ToolError::unavailable("store_unreachable", "run: doctor"),
            SE::Index(IndexError::Memory(_)) => ToolError::unavailable("memory_failed", "run: doctor"),
            SE::NoFacts(_) => ToolError::unavailable(
                "index_memory_divergence",
                "the index references a row memory does not have; run: hub index rebuild",
            ),
            // 偏离 brief 的新增支（必要，不是扩权）：`SearchError` 实际是 5 支（R28 裁定落地时
            // `Memory(#[from] MemError)` 补进来的，brief 的 match 写于 4 支的形状 ⇒ 缺它即 E0004 编译不过）。
            // 映射**委托给下面的 `From<MemError>`**，不在这里重述任何一支的真值表：
            // "分型映射只有一个家"这句注释对 Memory 载荷同样成立，就地展开就是第二份映射表。
            SE::Memory(e) => ToolError::from(e),
            // Store/Sql 两支**故意不映射成 Empty**：库读不到与库里没东西是两种分型，
            // 合并它的症状是"磁盘坏了被报成记忆为空"。
            // 【R45 修正本注释】点名顺序对**互斥支**不产生优先级：`Store(Io)`、`Store(Sql)`、
            // `Store(ReadOnly)`、`Store(SchemaTooNew)` 四支的匹配模式互斥，任何排列下每个值都只
            // 落进同一支——乱序既不 E0004 也不 unreachable_patterns、真值不变。这里逐变体点名的收益是
            // **形状**的（`StoreError` 加新变体当场 E0004 而不是被通配咽下），不是顺序的。
            SE::Store(rc_store::StoreError::ReadOnly(_)) => ToolError::authorization(
                "replica_read_only",
                "this handle is a replica",
            ),
            SE::Store(rc_store::StoreError::SchemaTooNew { .. }) => ToolError::unavailable(
                "schema_too_new",
                "memory.db was written by a newer build; do not mix binaries",
            ),
            // T3a：canonical 基线之前写的库（旧 12 表形状）**没有迁移**，`rc-store` 在开门时
            // 一律拒读并报 `SchemaRebuildRequired`。语义是"旧库，等操作员删/归档后重建"。
            //
            // 分型取 `Unavailable`（不是 `Validation`/`Authorization`：输入与身份都没问题，
            // 是"此刻这份库不能用"；也不是 `Empty`：库里可能满是旧行，那是"读不出来"不是"没有"）。
            // 码取既有的 `store_unreachable` 一族、hint 固定 `run: doctor`：
            // 下一跳动作与 `Io`/`Sql` 相同（先跑 doctor，由 doctor 点名"这份库要重建"），
            // 本 crate 不为此新造错误码（错误目录的键要稳定，新码属于改目录的工单，不属于 T3a）。
            // `SchemaTooNew` 的 `schema_too_new` 在这里**不能复用**：它讲的是"库比二进制新，别混用
            // 二进制"，与"库比二进制旧，去重建"是两个相反的 next_action。
            SE::Store(rc_store::StoreError::SchemaRebuildRequired { .. }) => ToolError::unavailable(
                "store_unreachable",
                "run: doctor",
            ),
            // 阶段 4 T4-A：`StoreError` 新增 `ReplicaMissing`（节点侧"副本文件还不在"）。
            // 逐变体点名的表**不许用 `_` 兜底**（见文件头那句"新增变体当场 E0004"），
            // 所以这里必须单独成行。
            //
            // **码与 hint 都不许并进 `store_unreachable` 那一族**：
            // "库读不到"（跑 doctor 查盘/查库）与"这台节点还没同步过"（再同步一次）
            // 是**两个不同的 next_action**——同族里 `index_not_built`（等一次重建）、
            // `index_layer_unreadable`（要诊断）、`memory_failed`（跑 doctor）各占一码，
            // 就是这条规矩的先例。合并的后果是操作员看到"store_unreachable"就去查磁盘，
            // 而实际上只要再同步一次。
            //
            // 码取 `no_memory`（与 `mcp --replica` 那条"无可用记忆"同码）：
            // 同一件事在两层上不许有两个码。
            SE::Store(rc_store::StoreError::ReplicaMissing) => ToolError::unavailable(
                "no_memory",
                "no snapshot on this node yet — run: node sync",
            ),
            // 裁定 R40：这两支**逐变体点名**而不是 `SE::Store(_)`。码与 hint 与拆分前逐字相同
            // （行为一字没改），补的是形状：`StoreError` 将来加一个变体，这里当场 E0004 而不是
            // 被兜底串咽下去——`Io` 与 `Sql` 在运维面上确实是同一个动作（跑 doctor），那是结论，
            // 不该由通配符替未来的变体预先做这个结论。
            SE::Store(rc_store::StoreError::Io(_)) => ToolError::unavailable("store_unreachable", "run: doctor"),
            SE::Store(rc_store::StoreError::Sql(_)) => ToolError::unavailable("store_unreachable", "run: doctor"),
            SE::Sql(_) => ToolError::unavailable("store_unreachable", "run: doctor"),
        }
    }
}

impl From<rc_memory::MemError> for ToolError {
    fn from(e: rc_memory::MemError) -> Self {
        use rc_memory::MemError as ME;
        match e {
            ME::ContentEmpty => ToolError::validation("content_empty", "title and body cannot be empty"),
            // brief 写的是 `ME::PronounAlias(_)`；实际变体名是 `AliasIsPronoun`（任务 3 落地定名）。
            // 机读码沿用 brief 的 `pronoun_alias`，分型与语义不变。
            ME::AliasIsPronoun(_) => ToolError::validation(
                "pronoun_alias",
                "pronouns are never aliases; use the entity name the human used",
            ),
            ME::AnchorNotHuman => ToolError::validation(
                "anchor_not_human",
                "a positive attestation must resolve to a human turn",
            ),
            // 载荷是 `MemKey`（`Display` 只吐 `key_string()`，身份值不含正文，守卫 12 已在构造点收口）。
            ME::NotFound(_) => ToolError::empty("row_absent", "the handle does not resolve; it may have been purged"),
            // brief 写的是 `ME::ReadOnly(what)`；实际的 `ReadOnly` 是无载荷单元变体
            // （任务 3 落地定形），所以这里连那个 `&'static str` 都没有——"零用户字节"只强不弱。
            ME::ReadOnly => ToolError::authorization(
                "replica_read_only",
                "this handle cannot write the memory store",
            ),
            // 偏离 brief 的新增支（必要）：实际 `MemError` 有 `ProvenanceMissing(&'static str)`
            // （X 写入者的输入约束：内容行没带出处），brief 的 match 没有它 ⇒ 非穷尽编译不过。
            // 分型取 Validation：它和 `ContentEmpty` 同族——都是"这次写入的要求本身不合法"，
            // 人下一步做的是改输入，不是改配置、等重建或承认库空。
            ME::ProvenanceMissing(_) => ToolError::validation(
                "provenance_missing",
                "every memory write carries at least one provenance turn; re-send with one",
            ),
            // brief 写的是 `ME::Sql(_) | ME::Store(_)`；`MemError` 实际没有 `Store` 支
            // （rusqlite 错误在构造点就折进消毒后的 `Sql(String)` 载荷），wildcard 因此收窄为一支。
            // v4 A1 的两支输入错误：和 `ContentEmpty` 同族，都是"这次写入的要求本身不合法"。
            // `BadA1Field` 的载荷是静态字段名（不含模型正文），hint 只指方向、不回显它。
            ME::BadA1Field(_) => ToolError::validation(
                "bad_a1_input",
                "an a1 field is invalid; re-extract this batch",
            ),
            ME::UnknownAnchorRef => ToolError::validation(
                "unknown_anchor_ref",
                "an a1 row references an anchor that is not in this batch; re-extract this batch",
            ),
            ME::Sql(_) => ToolError::unavailable("store_unreachable", "run: doctor"),
        }
    }
}

/// 索引侧来源错误的分型表（任务 8）。**`kind`、码与 hint 三个维度与上面 `From<SearchError>` 的
/// `SE::Index(..)` 各支逐字相同**（裁定 R40：这里刻意写成第二份表而不是委托——委托会让
/// `tests/tools_shape.rs::两个来源的分型映射不许漂` 结构上不可能失败，
/// 而一条从未红过的守卫等于没有守卫）。相同这件事现在是**逐臂有读者**：那张用例把本表的八臂
/// （`NotBuilt`、`NoQueryTokens`、`NormMismatch`、`LayerUnreadable`、`Sql`、`Memory`、
/// `ReadOnly`+`Store(ReadOnly)` 合并臂、`Store(_)` 兜底支）每臂各构造一次载荷，与
/// `SE::Index(同一支)` 对撞 `(kind, code, hint)` 三元组 ⇒ 两处漂一个字符（码漂、hint 漂、
/// 或某一臂的 kind 被改型）都红在点名的那一臂上。
/// 唯一的形状差：本表把 `ReadOnly` 与 `Store(ReadOnly)` 合成**一条或臂**，上面那张表把它们写成
/// **两支行** ⇒ 那是 1↔2 的三个读数（对撞用例两支都点了名），不是有一支对撞不上。
impl From<rc_index::IndexError> for ToolError {
    fn from(e: rc_index::IndexError) -> Self {
        use rc_index::IndexError as IE;
        match e {
            IE::NotBuilt => ToolError::unavailable(
                "index_not_built",
                "run: hub index rebuild --data-dir <dir>",
            ),
            IE::NormMismatch { .. } => ToolError::unavailable(
                "index_norm_stale",
                "analyzer changed; run: hub index rebuild --data-dir <dir>",
            ),
            IE::NoQueryTokens => ToolError::validation(
                "query_has_no_tokens",
                "the query normalizes to zero indexable tokens; send actual words",
            ),
            // 与 `SE::Index(ReadOnly)` / `SE::Index(Store(ReadOnly))` 两支同码同 hint。
            // 合并成一支配或臂是这张表的形状（两支的读数本来就同一个 next_action）；
            // `IE::Store(_)` 兜底支必须排在它之后——这两支**不**互斥，顺序在这里真有优先级。
            IE::ReadOnly(_) | IE::Store(rc_store::StoreError::ReadOnly(_)) => ToolError::authorization(
                "replica_read_only",
                "this handle is a replica; it cannot maintain its own index",
            ),
            // 裁定 R18/R24 的第八支：`LayerUnreadable` 是**无载荷**单元变体，专指
            // "索引层在但逐行读失败"。它与 `NotBuilt` 分家——"没建过"（运维能一步修）和
            // "坏了"（要诊断）是两个不同的 next_action。
            IE::LayerUnreadable => ToolError::unavailable("index_layer_unreadable", "run: doctor"),
            IE::Sql(_) => ToolError::unavailable(
                "index_failed",
                "run: hub index status --data-dir <dir>",
            ),
            // Store/Sql 两支**故意不映射成 Empty**（与 `From<SearchError>` 同一条理由）：
            // 库读不到与库里没东西是两种分型。
            IE::Store(_) => ToolError::unavailable("store_unreachable", "run: doctor"),
            IE::Memory(_) => ToolError::unavailable("memory_failed", "run: doctor"),
        }
    }
}
