//! agent 面的第二个门面（第一个是 `hook`）。**它不含判定逻辑**：记忆面十二个工具转调 `rc_agent`、
//! 核心层工具转调 `rc_core`，所以同一个工具在两个门面上的返回形状相同（守卫 3），
//! 而 `mcp` 自己看不见检索、闸门与密文。
use clap::Parser;
use rc_agent::TurnSink;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;

mod rpc;
use rpc::{InMsg, INVALID_PARAMS, METHOD_NOT_FOUND, PARSE};

#[derive(Parser)]
#[command(name = "recollect-mcp", version)]
struct Cli {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// 计数与转录归属的主语。中心模式**必填**（`--device-id` 或 `RECOLLECT_DEVICE_ID`）；
    /// 节点模式由设备私钥导出，给了就必须与钥匙一致。
    #[arg(long)]
    device_id: Option<String>,
    /// 节点模式：读下行整替落地的**副本**，写只许写本机转录层。
    /// 副本不在 ⇒ 进程照常起来（`remember` 要能用），读面显式说"无可用记忆"。
    #[arg(long)]
    replica: bool,
    /// core 层的主语（工具契约 §1.1）。给了就以它为准，`clientInfo.name` 只兜底。
    /// 过 `safe_token`：非法 ⇒ 退 3，不静默换成 `"unknown"`（那会把写入口记到错的人头上）。
    #[arg(long)]
    agent_id: Option<String>,
    /// 读者归属的 principal（W37-A1）：操作员在注册/换装时声明"这个 agent 归哪个
    /// principal"。给了它，读闸 4b 才会把同 principal 异 agent 的行放行；不给 ⇒
    /// 4b 永不命中（fail-closed），身份不许从 `clientInfo` 之类猜。与 `--agent-id`
    /// 同级、同 `safe_token` 检查。
    #[arg(long)]
    principal_id: Option<String>,
    /// `recollect_export` 的运维开关。**默认关**：导出是 user/operator 的动作（契约 §3/§5）。
    #[arg(long)]
    enable_export: bool,
}

fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) if e.use_stderr() => {
            return operational("invalid invocation — usage: recollect-mcp [--data-dir DIR] --device-id ID");
        }
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(0);
        }
    };
    // 清单只有一个来源：`capabilities().tools`。这里只做**对撞**——少一个定义就拒绝启动，
    // 而不是到 `tools/list` 时少给一个工具（后者是静默的，而"宿主看不见某个工具"没有报错面）。
    let defs = match tool_defs() {
        Some(d) => d,
        None => return operational("tool definitions out of sync with capabilities — run: doctor"),
    };
    // core 的主语：启动配置显式给，形状不过 `safe_token` 就退 3（与设备号同一把尺子）。
    // 相等比较而不是采信返回值：理由同下面设备号那一行。
    let cli_agent = match cli.agent_id.as_deref() {
        Some(a) if rc_contract::safe_token(a) == a => Some(a.to_string()),
        Some(_) => return operational("agent id is not a safe token — run: doctor"),
        None => None,
    };
    // 读者的归属（W37-A1）：同一把尺子。没有它 4b 永不命中——那是 fail-closed 的方向，
    // 不报错、只是"同 principal 互读"这一档不生效。
    let cli_principal = match cli.principal_id.as_deref() {
        Some(p) if rc_contract::safe_token(p) == p => Some(p.to_string()),
        Some(_) => return operational("principal id is not a safe token — run: doctor"),
        None => None,
    };
    let (root, device) = if cli.replica {
        match open_node(&cli) {
            Ok(pair) => pair,
            Err(msg) => return operational(msg),
        }
    } else {
        // 主语不能猜。`hub` 那边猜不出来会落 "unknown"，那是**导入审计行**、旁边有 `loc` 可查；
        // 这里的主语要活在一整张计数器表里，一个猜错的设备号比空主语更难查。
        // 注意是**相等比较**而不是采信 `safe_token` 的返回（它的拒绝值自己不是合法 token）。
        let device = match cli.device_id.or_else(|| std::env::var("RECOLLECT_DEVICE_ID").ok()) {
            Some(d) if rc_contract::safe_token(&d) == d => d,
            Some(_) => return operational("device id is not a safe token — run: doctor"),
            None => return operational("missing --device-id (or RECOLLECT_DEVICE_ID)"),
        };
        let root = match rc_store::Root::open(&cli.data_dir) {
            Ok(r) => r,
            Err(_) => return operational("cannot open data store — run: doctor"),
        };
        (Store::Center(root), device)
    };
    // E2（A31）的迁移提示：开库可能先把 v3 库迁到 v4。mcp 也可能是最先开库的入口，
    // 静默迁移在这里就等于没打印。**stdout 是协议**（一行一个 JSON-RPC 报文），所以这条
    // 只许走 stderr；文本的唯一出处是 `MigrationReport::ledger_warning`，这里只管接线。
    // 前缀 `warning:`、不改退出码：迁移不是故障。
    if let Some(msg) = root.migration_report().ledger_warning() {
        let _ = std::io::stderr().write_all(format!("warning: {msg}\n").as_bytes());
    }
    // core 层打不开**不阻止进程启动**：记忆面与转录面照常可用，`core_*` 一律回
    // `core_unavailable`（工具契约 §7）。把"这一层坏了"升级成"整个门面起不来"
    // 会让一次 core.db 损坏同时关掉 recall/remember。
    let core = rc_core::CoreStore::open(&cli.data_dir).ok();
    let agent = cli_agent.clone().unwrap_or_else(|| "unknown".into());
    let server = Server {
        root,
        device,
        defs,
        agent: RefCell::new(agent),
        booted: RefCell::new(false),
        core,
        cli_agent,
        principal: cli_principal,
        allow_export: cli.enable_export,
    };
    run_loop(&server)
}

/// 退出码 3（运维失败）。`hook` 那边同名同义：**进程起不来时说的是一句话，不是栈**。
fn operational(msg: &str) -> ExitCode {
    let _ = std::io::stderr().write_all(format!("error: {msg}\n").as_bytes());
    ExitCode::from(3)
}

/// `recollect_search` 的 audit_log detail 里 `q` 的截断上限（细案 §二.5：查询串入库
/// 与转录同库同敏感级，不另行脱敏；截 120 字符只是账行的大小刻度）。
const SEARCH_AUDIT_QUERY_CHARS: usize = 120;

/// 门面底下握着的那三样。**中心模式与节点模式是两种形状**，
/// 所以它们是变体而不是"同一个 `Root` 加一个旗标"——
/// 节点模式没有可写的记忆库，把中心那套形状硬套上去就等于给了它写副本的能力。
enum Store {
    Center(rc_store::Root),
    Node(rc_store::NodeRoot),
}

/// 节点模式的记忆句柄有两种来源：中心借、节点按调用打开。
enum Replica<'a> {
    Borrowed(&'a rc_store::MemoryHandle),
    Owned(rc_store::MemoryHandle),
}

impl std::ops::Deref for Replica<'_> {
    type Target = rc_store::MemoryHandle;
    fn deref(&self) -> &Self::Target {
        match self {
            Replica::Borrowed(h) => h,
            Replica::Owned(h) => h,
        }
    }
}

/// 每次工具调用都按状态文件重开：副本是另一个进程换的，缓存句柄既看不见换代，
/// 也会在 Windows 上把新副本的 rename 挡住。
fn node_memory(root: &rc_store::NodeRoot) -> Option<rc_store::MemoryHandle> {
    for _ in 0..2 {
        let path = if node::staging::is_active(root.data_dir()) {
            node::staging::path(root.data_dir())
        } else {
            node::state::current_replica_path(root.data_dir())?
        };
        match rc_store::open_replica(&path) {
            Ok(h) => return Some(h),
            // 读状态与打开之间刚好换代：重读一次。
            Err(rc_store::StoreError::ReplicaMissing) => continue,
            Err(_) => return None,
        }
    }
    None
}

impl Store {
    /// 读侧的记忆句柄。**节点模式没有副本时是 `None`**，
    /// 调用方要显式说"无可用记忆"，不许把它折成"库里没有"。
    fn memory(&self) -> Option<Replica<'_>> {
        match self {
            Store::Center(r) => Some(Replica::Borrowed(r.memory())),
            Store::Node(r) => node_memory(r).map(Replica::Owned),
        }
    }
    fn transcript(&self) -> &rc_store::TranscriptHandle {
        match self {
            Store::Center(r) => r.transcript(),
            Store::Node(r) => r.transcript(),
        }
    }
    fn ledger(&self) -> &rc_store::LedgerHandle {
        match self {
            Store::Center(r) => r.ledger(),
            Store::Node(r) => r.ledger(),
        }
    }
    fn data_dir(&self) -> &std::path::Path {
        match self {
            Store::Center(r) => r.data_dir(),
            Store::Node(r) => r.data_dir(),
        }
    }
    fn migration_report(&self) -> &rc_store::ddl::MigrationReport {
        match self {
            Store::Center(r) => r.migration_report(),
            Store::Node(r) => r.migration_report(),
        }
    }
    fn is_node(&self) -> bool {
        matches!(self, Store::Node(_))
    }
}

/// 节点模式的装配。**设备 id 由私钥导出**，不取 `--device-id`。
///
/// 副本不在时**照样起来**：`remember` 写的是本机转录层，它不该被一次还没发生的
/// 同步挡住。副本句柄**不在这里持有**——副本是另一个进程换代落的，句柄按调用打开
/// （见 `Store::memory`），长驻进程才不会停在开机时那一代。
fn open_node(cli: &Cli) -> Result<(Store, String), &'static str> {
    // 主语由私钥导出。配对时中心就是按这把钥匙的公钥导出设备 id 的，
    // 两者必须是同一个——否则本机写下的轮带着另一个设备名，上行时会被中心按
    // "署名不符"逐条拒收，而症状长得像"中心不认我"。
    let device_key_path = cli.data_dir.join("device.key");
    let key = match rc_net::key::DeviceKey::load(&device_key_path) {
        Ok(k) => k,
        Err(_) => return Err("no device key — run: node pair"),
    };
    let device = key.device_id();
    if let Some(given) = cli.device_id.as_deref() {
        if given != device {
            return Err("device id does not match the device key — run: doctor");
        }
    }
    // 中心快照不带索引层；节点读面前先给当前副本建一份索引副本。
    let manifest = node::state::load(&node::state::state_path(&cli.data_dir))
        .map_err(|_| "the node state is malformed — run: doctor")?
        .map(|s| s.current_manifest)
        .unwrap_or_default();
    if node::index::ensure_current_replica(&cli.data_dir, &manifest, now_ms()).is_err() {
        return Err("the replica index cannot be built — run: doctor");
    }
    let root = match rc_store::NodeRoot::open_node(&cli.data_dir, None) {
        Ok(r) => r,
        Err(_) => return Err("cannot open the node store — run: doctor"),
    };
    Ok((Store::Node(root), device))
}

/// 时钟。V14 的排除清单里有 `crates/mcp/src/main.rs`。**不给 `--now-ms` 是有意的**：
/// 长驻服务里注入一个冻结的"现在"会让过期与冷却判定失真，而阶段 5 的对拍跑的是 `hook`。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

struct Server {
    root: Store,
    device: String,
    defs: Vec<Value>,
    agent: RefCell<String>,
    booted: RefCell<bool>,
    /// core 层句柄。打不开 ⇒ `None`，工具面回 `core_unavailable`（不阻止启动）。
    core: Option<rc_core::CoreStore>,
    /// 启动配置给的 core 主语。**CLI 优先**：它在场时 `clientInfo.name` 不许覆盖。
    cli_agent: Option<String>,
    /// 启动配置给的读者归属（W37-A1）。`None` ⇒ 4b 永不命中（fail-closed）。
    principal: Option<String>,
    /// 默认 false：`recollect_export` 是 user/operator 的动作（契约 §3/§5）。
    allow_export: bool,
}

impl Server {
    /// 读者身份（W37-A1）：agent 面 = 宿主自报的 agent 名 + 启动配置声明的 principal。
    /// 所有 agent 面的 `Viewer` 构造都走这里——"带 principal"不许在哪一处被忘掉。
    fn agent_viewer(&self) -> rc_agent::recall::Viewer {
        let v = rc_agent::recall::Viewer::agent(self.agent.borrow().clone());
        match &self.principal {
            Some(p) => v.with_principal(p.clone()),
            None => v,
        }
    }

    /// 同上，Human 面（导出）。principal 同样要带上：visibility=person 那一格的
    /// 判据是 `surface == Human && principal_eq`。
    fn human_viewer(&self) -> rc_agent::recall::Viewer {
        let v = rc_agent::recall::Viewer::human(self.agent.borrow().clone());
        match &self.principal {
            Some(p) => v.with_principal(p.clone()),
            None => v,
        }
    }

    /// `recollect_search` 的参数解析 + 一次调用。参数校验的家在工具面（schema 只是
    /// 声明，见 `input_schema` 的档注）；limit 的缺省与收口住库层（`0` = 用缺省），
    /// 这里不抄第二把尺。
    fn transcript_search(
        &self,
        args: &Value,
    ) -> Result<rc_agent::tools::SearchReadout, rc_agent::ToolError> {
        let query = required_str(args, "query")?;
        let limit = transcript_limit(args, 20, "limit <= 20")?;
        rc_agent::tools::search(self.root.transcript(), &self.agent_viewer(), query, limit)
    }

    /// `recollect_search` 的**本地实现 + 审计**（原本住在臂内；W39 刀 D 抽出来，
    /// 因为远端回落那一支要复用同一段）。远端成功那一支不走这里：
    /// 中心在它那一侧已经落了 `transcript_read` 审计行。
    fn local_search_readout(
        &self,
        args: &Value,
        now: i64,
    ) -> Result<rc_agent::ToolJson, rc_agent::ToolError> {
        // audit_log：**每次本地调用写一行**（细案 §二.5）。先跑后留痕——hits/truncated
        // 是这次调用的读数；校验失败的那一次也是一次调用（hits=0 照写）。
        let out = self.transcript_search(args);
        let raw_q = args.get("query").and_then(Value::as_str).unwrap_or_default();
        let (hits, truncated) = match &out {
            Ok(r) => (r.hits, r.truncated),
            Err(_) => (0, false),
        };
        let detail = json!({
            "q": raw_q.chars().take(SEARCH_AUDIT_QUERY_CHARS).collect::<String>(),
            "hits": hits,
            "truncated": truncated,
        })
        .to_string();
        // subject = viewer principal 或 '-'。账是账，答案是答案：留痕失败不影响
        // 回执（与 `bump` 同一条纪律）。
        let _ = rc_ledger::device::append_audit(
            self.root.ledger(),
            now,
            &self.caller_label(),
            "search",
            Some(self.principal.as_deref().unwrap_or("-")),
            &detail,
        );
        out.map(|r| r.envelope)
    }

    /// W39 刀 D：远端转录读的**审计头**（细案 §3.1）：`x-recollect-agent` 恒带，
    /// `x-recollect-principal` 配了才带。两格都**不进签名**（canonical 只盖
    /// method/target/正文 sha/device/nonce/epoch），中心只把它们写进 `transcript_read` 审计。
    fn remote_transcript_extra(&self) -> Vec<(String, String)> {
        let mut extra =
            vec![("x-recollect-agent".to_string(), self.agent.borrow().clone())];
        if let Some(principal) = &self.principal {
            extra.push(("x-recollect-principal".to_string(), principal.clone()));
        }
        extra
    }
}

/// W39 刀 D：远端故障 → 回落信封的两格（`degraded` + 封闭码）。
/// `NotPaired` 是"本机还没配对"，**不是故障**：本地读数不降级、不带 `remote_error`
/// （细案 §3.2/§五.6）；其余三种都是"本该读中心、这次没读到"，显式降级。
fn transcript_fallback(
    fault: node::remote_transcript::RemoteFault,
) -> (bool, Option<&'static str>) {
    match fault {
        node::remote_transcript::RemoteFault::NotPaired => (false, None),
        other => (true, Some(other.slug())),
    }
}

/// 节点面的**本地回落信封**：`source:"local"` + 如实的两格（细案 §3.2）。
fn local_transcript(
    j: rc_agent::ToolJson,
    fault: node::remote_transcript::RemoteFault,
) -> String {
    let (degraded, code) = transcript_fallback(fault);
    j.with_transcript_source("local", degraded, code).text()
}

impl TurnSink for Server {
    /// `remember` 的那条 agent 轮落到转录库。**没有 `role` 形参**（任务 8 的 compile-fail 用例守着），
    /// 所以角色在这里、也只有这里被写成 `"agent"`。
    fn append_agent_turn(&self, session_key: &str, body: &str, now: i64) -> Result<String, rc_agent::ToolError> {
        if rc_contract::safe_token(session_key) != session_key {
            // 会话键是宿主可控文本：`"C:/…/transcript.db"` 过得了 normalize 也过不了路径检查。
            // `remember` 那边查的是"空"，这里查的是"形状"，两件事不同，所以不算规则写两遍。
            return Err(rc_agent::ToolError::validation(
                "bad_session_key",
                "session_key must be a short token; run: doctor",
            ));
        }
        // 长度与空判定已经在 `RememberRequest::new` 里做过，这里**不查第二遍**：
        // 一份规则写两遍就是下一次漂移（守卫 3 同族）。
        let body = rc_contract::normalize_body(body);
        let uid = rc_contract::compute_turn_uid(&self.device, session_key, "agent", &body, now);
        let turn = rc_contract::NormalizedTurn {
            tier: rc_contract::TurnTier::ContentOnly,
            turn_uid: uid.clone(),
            body_fp: rc_contract::compute_body_fp(&body),
            body,
            role: "agent".into(), // 唯一的落点。它不是形参，所以它只能是这个字面量。
            created_at_ms: now,
        };
        // W39 刀 E（细案 §二.4）：agent 轮写 principal——`--principal-id` 给了就带上
        // （`insert_batch` 同事务盖章并镜像回 `sessions.owner_principal_id`），没给仍 NULL。
        // 这一条**推翻 W37「MCP 侧不写 principal 进转录行」的旧口径**（用户 2026-11-09 裁定）：
        // 旧口径下边缘 agent 的 remember 轮在中心无主人，读回来答不出"谁在跟它说话"。
        let sid = rc_contract::SessionId {
            session_key: session_key.to_string(),
            device_id: self.device.clone(),
            source_agent: self.agent.borrow().clone(),
            machine: None,
            project_hint: None,
            last_saved_at_ms: Some(now),
            owner_principal_id: self.principal.clone(),
            principal_name: None,
            principal_source: Some("agent_runtime".into()),
        };
        // 撤掉 `RefCell<Root>`（任务 12 Step 3c）：`root.transcript()` 已是 `&self` 访问器，
        // 而 `rc_ledger::insert_batch` 的形参 `&TranscriptHandle` 也是 `&`（本机已核 `&mut` 零命中），
        // 所以这里直接 `&self.root.transcript()` 而不借 `&mut`。
        rc_ledger::insert_batch(self.root.transcript(), &sid, std::slice::from_ref(&turn), now)
            .map_err(store_down)?;
        Ok(uid)
    }
}

/// `StoreError` → `ToolError` 的唯一映射（本文件内）。它的码与 hint 与既有的那一处**同一个常量对**，
/// 而既有那处的 `.map_err(|_| …)` 闭包在这一刻改调本函数 ⇒ 判据断这一对码与提示串在本文件只许出现一次。
/// 为什么不借 `From<StoreError> for rc_search::SearchError` 再借 `From<SearchError> for ToolError`：
/// 那条路要在本文件出现 `rc_search` 这个词，而阶段 1 出口判据 6e 与 §2.20 禁令 2 都禁它（P-47）。
fn store_down(_e: rc_store::StoreError) -> rc_agent::ToolError {
    rc_agent::ToolError::unavailable("store_unreachable", "run: doctor")
}

impl rc_agent::CycleRead for Server {
    /// P2 的缝实现方：两个库各读一次，产出一个完整读数，**不做任何分型**（§2.20）。
    /// 取的是共享借用而不是独占借用——这条缝一行都不写。它与 `append_agent_turn` 的
    /// 独占借用在同一次 `remember` 里**顺序**发生而不是嵌套，所以任务 10 的形状不撞 P-48；
    /// 撞车的是任务 12（那时 `remember` 自己也要一把 `&MemoryHandle`，外层 `Ref` 还没结束）。
    fn turn_extraction(&self, turn_uid: &str) -> Result<Option<rc_agent::TurnExtraction>, rc_agent::ToolError> {
        let origin = rc_ledger::transcript::turn_origin(self.root.transcript(), turn_uid)
            .map_err(store_down)?;
        // 幽灵句柄：`Ok(None)`，分型由 `rc-agent` 给（P-62）。
        let Some(origin) = origin else { return Ok(None) };
        let record = rc_ledger::water::extract_record(self.root.ledger(), turn_uid).map_err(store_down)?;
        // 缺行 = **还没蒸到**，不是"读不到"。默认值走枚举、不走 `"pending"` 字面量（§2.22 连带：
        // 词表的家在 `ExtractState::slug()`，写默认的人应当用那个枚举）。
        let state = record.clone().map(|r| r.state).unwrap_or(rc_ledger::water::ExtractState::Pending);
        let reason = record.and_then(|r| r.reason);
        // 键形来自水位层那把唯一的拼键函数（P-63）——**这是本文件第二次也是最后一次拼那把键**。
        // 设备一栏取**出处设备**（`origin.device_id`），不是本机设备：别的设备导进的会话
        // 由那台设备的 scope 蒸发，本机的蒸馏游标替它答不上话（A24/A31）。
        // 家族名同样不抄字面：`water::DISTILL` 是它的家。
        let scope = rc_ledger::water::distill_scope(&origin.device_id, &origin.session_key);
        let cursor = rc_ledger::water::read(self.root.ledger(), rc_ledger::water::DISTILL, &scope)
            .map_err(store_down)?;
        // 无游标 ⇒ `i64::MIN`：整个会话都还没蒸过，于是队列位置是"这条轮之前的全部"。
        // 右端点同样用**到达钟**（`origin.ingested_at_ms`）：设备钟早的迟到行照样在队列里。
        let eta_turns = rc_ledger::transcript::count_between(
            self.root.transcript(),
            &origin.session_key,
            cursor.map(|c| c.at_ms).unwrap_or(i64::MIN),
            origin.ingested_at_ms,
        )
        .map_err(store_down)?;
        Ok(Some(rc_agent::TurnExtraction {
            extract_state: state.slug().to_string(),
            reason,
            eta_turns,
        }))
    }
    /// P5 的缝实现方：每个数**各读各的家**，一个都不自己造。
    /// 这里的两次聚合（求和、相减）是**裸读数聚合**而不是判定：S3a 起"配额烧完"那一格
    /// 只是运行记录的搬运（`quota_stopped`），累计和另外命名成成本读数
    /// （`llm_calls_total`），两者在同一份 `rows` 上一次读出来。`lag_turns` 用 `saturating_sub`：
    /// 终态行数理论上不超过转录行数，但 `purge` 之后 `extract_pass_state` 留残留行是这个上限
    /// 被踩破的唯一现实路径，而那时的正确读数就是"没有落后"，不是"负数落后"，也不是报错——
    /// 报错会把一条**运维可修的脏行**变成 agent 面上的一次失败。
    ///
    /// S3b：lag 与最后时刻**只读 `extract_pass_state`**（终态）。legacy `extract_state` 今天
    /// 一行都不写（写它的是旧路径），而每天的抽取路径写的是 pass 表；继续读 legacy 会让这一格
    /// 永远是 361。也不做两表并集/取大：并集会把"只跑完半套 pass 的轮"算成整轮完成，
    /// 那是这一格最大的假绿源。
    fn cycle_health(&self) -> Result<rc_agent::CycleHealth, rc_agent::ToolError> {
        let rows = rc_ledger::counters::read_all(self.root.ledger()).map_err(store_down)?;
        // 度量名不写字面量，用 rc-llm 立的那个常量（名字就在下面那行调用里，这里不复述——判据在数它）。
        // 不筛 subject_kind 是有意的——`llm_calls` 这一支只由任务 9 的 `charge_counters` 写在
        // `llm_job` 主语上（唯一的写入者），多筛一层就是把"谁写过它"这条事实抄进门面（守卫 3）。
        let used: i64 = rows.iter()
            .filter(|r| r.metric == rc_llm::ALERT_METRIC)
            .map(|r| r.count)
            .sum();
        // S3a/R3：同一份 `rows` 上再判两件事，不新增查询。
        // ① `quota_stopped`：最近一次运行是否触顶（判定，三态）。
        // ② `llm_calls_total`：累计成本读数。**没有 `llm_calls` 行 ⇒ `None`**，
        //    而不是 0（"跑过且一次没花"是另一个读数）。
        let quota_stopped = rc_ledger::counters::quota_stopped(&rows);
        let llm_calls_total = rows.iter()
            .any(|r| r.metric == rc_llm::ALERT_METRIC)
            .then_some(used);
        // 落后格的分母是**可抽取轮**（`role <> 'system'`），与抽取候选 SQL 同一条判据；
        // system 轮只作上下文、永不作 TARGET，也就永不会有终态行。用总行数当分母会留下一个
        // 恒等于 system 轮数的假积压（2026-11-04 本机：361 总轮、258 条 system ⇒ lag 恒 258）。
        // 相减后**钳到零**（G19 落地时顺带纠正）：`saturating_sub` 饱和的是 `i64` 边界不是 0，
        // 残留终态行 > 可抽取轮时它照样给负数——与上面注释"正确读数是'没有落后'"自相矛盾。
        // `ingest_status` 的 `turns_pending` 用的是同一形状（`rc-agent/tools.rs`）。
        let total = rc_ledger::transcript::count_extractable_turns(self.root.transcript())
            .map_err(store_down)?;
        let seen: i64 = rc_ledger::pass_state::count_terminal_turns(self.root.ledger())
            .map_err(store_down)?;
        let lag_turns = (total - seen).max(0);
        let last = rc_ledger::pass_state::last_terminal_at_ms(self.root.ledger())
            .map_err(store_down)?;
        Ok(rc_agent::CycleHealth {
            lag_turns,
            last_success_at_ms: last,
            // C2 / dormant：这一格今天零读者（判定吃 `quota_stopped`，成本读 `llm_calls_total`），
            // 保留字段不删；填的还是同一个 `used`，别再往它身上长判定。
            // 同族的"单轮上限"那一格 2026-11-06 已随"砍掉调用配额"整格删除（上限本身没了）。
            llm_calls_used: used,
            // P6 的两笔**都是账上的**，不是库里的行数：一列是最后成功时刻，一列是批末位置。
            // 行数由 `status` 现算（判据 6e/5：mcp 的正式依赖看不见 rc-memory）。
            // `derive` 游标的 `turn_uid` 列存的是"批末行的 key_string"（T4 第 4 步），
            // 形状正好是 `count_rows_since` 要的 (ms, "<tbl_prefix>:<id>")，所以这里整对搬走、不拆。
            last_derive_at_ms: rc_ledger::water::last_derive_at_ms(self.root.ledger())
                .map_err(store_down)?,
            derive_cursor: rc_ledger::water::read(self.root.ledger(), "derive", "")
                .map_err(store_down)?
                .map(|c| (c.at_ms, c.turn_uid)),
            // attention 扫描的心跳：它**只在账上**（`job_watermark` 的 `last_success_at_ms`），
            // 所以搬运走的是 `water::last_extract_at_ms` 那把通用的按家族名的读法（守卫 3——
            // 同一个数不许有第二个家），本文件一行 `rc_memory` 都不点（判据 6e / 5 管的就是这个面）。
            // `None` 只说一件事："这一族从没成功推进过"，不是"很久以前"，也不是扫描出错了
            // （出错由 hub 的退出码与 doctor 的 alerts 各自说话）。这一族只有 `ANOMALY_SCOPE`
            // 那一条空 scope 线，所以"跨 scope 取 max"对它等价于读那一行。
            last_anomaly_scan_at_ms: rc_ledger::water::last_extract_at_ms(
                self.root.ledger(), "anomaly_scan").map_err(store_down)?,
            quota_stopped,
            llm_calls_total,
        })
    }
    // A1 的两格：直读水位层那把唯一的读法（守卫 3——同一个数不许有第二个家），
    // 本文件不做任何判定，也不碰 `turn` / `entity` 的参数面。
    fn halted_scopes(&self) -> Result<i64, rc_agent::ToolError> {
        rc_ledger::water::halted_scopes_count(self.root.ledger()).map_err(store_down)
    }
    fn lag_scopes(&self) -> Result<Vec<(String, i64)>, rc_agent::ToolError> {
        rc_ledger::water::lag_by_scope(self.root.ledger(), self.root.transcript()).map_err(store_down)
    }
    /// P0-1：有效章覆盖的会话数。`rc-agent` 拿不到转录柄（守卫 6），只能经这道缝搬。
    /// 读失败照旧折成 `store_unreachable`（与上面三条同一个 `store_down`，不新开码）。
    fn principal_claims(&self) -> Result<Option<i64>, rc_agent::ToolError> {
        rc_ledger::transcript::principal_claim_sessions(self.root.transcript())
            .map(Some)
            .map_err(store_down)
    }
    /// G19 刀 4：会话两阶段读数的缝实现。每个数各读各的家，一个都不自己造：
    /// - 会话的轮清单：`sessions_after`（转录层按到达钟挑全部、按会话归拢——守卫 3：
    ///   "按会话归拢"只有这一条读法，不为本面重抄一遍 SQL）。诊断面的量级与 `status`
    ///   的全量计数同阶（O(库)），这是可接受的代价，换的是零新依赖、零第二把尺。
    /// - 可抽取判据：`role <> 'system'`（与 `lag_by_scope` / `count_extractable_turns`
    ///   同一条判据——system 轮永不写终态行，数进分母就是假积压）。
    /// - 终态：`pass_state::rows_for_turns` 按轮批量读（H7c1 接管对账的同一把读法）；
    ///   终态闭集与 `count_terminal_turns` 同一个家（`terminal_state` 对表
    ///   `TERMINAL_STATES`，不抄第二份闭集）。
    /// - `last_arrival_ms` 的口径是**全部轮**的 `max(ingested_at_ms)`（到达钟；
    ///   "我的导入到没到"是转录层的事实，与可抽取无关——两格口径的分工写在
    ///   `rc_agent::IngestCounts` 的档注里）。
    ///   `Ok(None)` 只在会话真不在时回（查无）；读失败走 `store_down`（P-54 同律）。
    fn ingest_counts(
        &self,
        session_key: &str,
    ) -> Result<Option<rc_agent::IngestCounts>, rc_agent::ToolError> {
        let sessions = rc_ledger::transcript::sessions_after(self.root.transcript(), i64::MIN, "")
            .map_err(store_down)?;
        let Some(s) = sessions.iter().find(|s| s.session_key == session_key) else {
            return Ok(None);
        };
        let extractable: Vec<&rc_ledger::transcript::LocalTurn> =
            s.turns.iter().filter(|t| t.role != "system").collect();
        let uids: Vec<String> = extractable.iter().map(|t| t.turn_uid.clone()).collect();
        let rows = rc_ledger::pass_state::rows_for_turns(self.root.ledger(), &uids)
            .map_err(store_down)?;
        let distilled = rows
            .iter()
            .filter(|r| terminal_state(r.state))
            .map(|r| r.turn_uid.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len() as i64;
        Ok(Some(rc_agent::IngestCounts {
            turns_total: extractable.len() as i64,
            distilled,
            last_arrival_ms: s.turns.iter().map(|t| t.ingested_at_ms).max(),
        }))
    }
}

/// 终态闭集的**唯一家**是 `pass_state::TERMINAL_STATES`（SQL 片段）。这里拿
/// `PassState::slug()` 逐个与它对表，不在本文件重抄第二份七态清单——
/// `count_terminal_turns`（`status.distill_lag_turns` 的减数）读的就是这一个家，
/// 两处漂开时 ingest_status 与 lag 会各自为政。带引号比对：片段里每个 slug 都
/// 单引号包裹，`'done'` 与 `'quarantined'` 互不为子串。
fn terminal_state(s: rc_ledger::pass_state::PassState) -> bool {
    use rc_ledger::pass_state::TERMINAL_STATES;
    TERMINAL_STATES.contains(format!("'{}'", s.slug()).as_str())
}

fn run_loop(server: &Server) -> ExitCode {
    let stdin = std::io::stdin();
    let mut lines = BufReader::new(stdin.lock());
    let out = std::io::stdout();
    let mut out = out.lock();
    loop {
        let mut line = String::new();
        match lines.read_line(&mut line) {
            Ok(0) => return ExitCode::from(0), // EOF = 宿主关了管道 = 正常收工
            Ok(_) => {}
            Err(_) => return operational("stdin read failed"),
        }
        // **一条输入恰好一条输出**，除了空行与通知。写两行等于把两个响应劈成一个，
        // 宿主会先看到半个；回给通知则会被当成某个还没发出去的请求的回执。
        let reply = match rpc::decode(&line) {
            InMsg::Empty | InMsg::Notification => None,
            InMsg::Malformed(reason) => Some(rpc::error_line(&Value::Null, PARSE, reason)),
            InMsg::Request { id, method, params } => Some(match handle(server, &method, &params) {
                Ok(result) => rpc::result_line(&id, result),
                Err((code, msg)) => rpc::error_line(&id, code, &msg),
            }),
        };
        if let Some(r) = reply {
            if writeln!(out, "{r}").is_err() {
                return ExitCode::from(0); // 宿主已经走了
            }
            let _ = out.flush();
        }
    }
}

fn handle(server: &Server, method: &str, params: &Value) -> Result<Value, (i64, String)> {
    let now = now_ms();
    match method {
        "initialize" => {
            let echoed = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("2025-03-26")
                .to_string();
            // CLI 给的 `--agent-id` 优先：它在场时 `clientInfo.name` **不覆盖** `server.agent`。
            // 自报家门只作兜底（契约 §1.1：`clientInfo.name` 只作显示与兜底，以启动配置为准）。
            if server.cli_agent.is_none() {
                if let Some(n) = params.get("clientInfo").and_then(|c| c.get("name")).and_then(Value::as_str) {
                    // `source_agent` 来自宿主的自报家门（MCP 的 initialize 里就有这个字段），
                    // 所以 `remember` 落的轮能区分是哪个 agent 写的——零配置。
                    // 相等比较而不是采信返回值：见本档开头偏离第 3 条。
                    *server.agent.borrow_mut() = if rc_contract::safe_token(n) == n { n.to_string() } else { "unknown".into() };
                }
            }
            Ok(json!({
                "protocolVersion": echoed,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "recollect-mcp", "version": env!("CARGO_PKG_VERSION")},
            }))
        }
        "tools/list" => Ok(json!({"tools": server.defs.clone()})),
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                // 这一条**是**协议层的不合法：这条消息本身没写要调用什么。
                return Err((INVALID_PARAMS, "tools/call needs a tool name".to_string()));
            };
            let args = params.get("arguments").cloned().unwrap_or(Value::Null);
            let (text, is_err) = server.invoke(name, &args, now);
            let mut content = vec![json!({"type": "text", "text": text})];
            if server.take_bootstrap() {
                // P1：本进程首次工具调用附带状态块。构造器与 `hook brief` 同一个（任务 9 Step 2）。
                // 节点模式没有副本时**不附**：零值状态块长得像一份正常读数，
                // 而"还没有副本"必须由读面显式说出口。
                if let Some(m) = server.root.memory() {
                    let j = rc_agent::tools::status(
                            &m,
                            Some(server),
                            &rc_agent::tools::StatusOptions::default(),
                            now,
                        )
                        .map(|t| t.text())
                        .unwrap_or_else(|e| e.to_string());
                    let boot = rc_agent::surface::status_block(&j).text();
                    content.push(json!({"type": "text", "text": boot}));
                }
            }
            server.bump(name, now);
            Ok(json!({"content": content, "isError": is_err}))
        }
        "ping" => Ok(json!({})),
        // 消息名不认识的才走协议错误；**工具名不认识走 isError**（偏离第 1 条）。
        other => Err((METHOD_NOT_FOUND, format!("method not found ({} chars)", other.len()))),
    }
}

impl Server {
    /// 记忆面与核心层工具的唯一分发点。返回 `(text, is_error)`：`is_error` 来自 `ToolError`，
    /// 而**协议级**失败由 `handle` 用 JSON-RPC 错误对象表达——两套永远不混。
    ///
    /// `ToolError` 里只有**三格**是正常状态、不标 `is_error`：`Empty` 型的 `no_rows` /
    /// `no_episode` / `no_edges`（见 [`is_recall_empty_readout`]，Phase 4 的 I-5b 把后两码接上）。
    /// 除此之外 `(text, true)` 的映射一字不改。
    ///
    /// 本档不再有借用：`Root` 在构造时一次性移入，本任务 12 把 §2.21 的 `RefCell` 撤了。
    fn invoke(&self, name: &str, args: &Value, now: i64) -> (String, bool) {
        match self.call_tool(name, args, now) {
            Ok(text) => (text, false),
            // `Display` = `kind:code hint`，三段全是常量：翻的只有 `is_error` 那一维，文本一字不动。
            Err(e) => (e.to_string(), !is_recall_empty_readout(&e)),
        }
    }

    fn call_tool(&self, name: &str, args: &Value, now: i64) -> Result<String, rc_agent::ToolError> {
        // 身份由运行时绑定，**参数里没有 `agent_id`**（契约 §1.1/§10.3）。这个键一旦出现就是
        // 误用：拒绝并留 `core_access_log`（denied 必须留痕），不回显它的值。
        if args.get("agent_id").is_some() {
            self.log_core(self.caller_label().as_str(), name, "denied", Some("agent_id parameter"), now);
            return Err(rc_agent::ToolError::validation(
                "agent_id_forbidden",
                "agent id is bound by the runtime",
            ));
        }
        if name.starts_with("core_") {
            return self.call_core(name, args, now);
        }
        match name {
            "recollect_recall" => {
                let view = args.get("view").and_then(Value::as_str).unwrap_or("brief");
                if !matches!(view, "brief" | "json" | "card" | "expand"
                                   | "overview" | "agenda" | "timeline") {
                    return Err(rc_agent::ToolError::validation(
                        "bad_view",
                        "view must be brief|json|card|expand|overview|agenda|timeline"));
                }
                // ── Phase 4（I-5b）的三个可选参数。校验点写在 `call_tool`，**不写在 schema**：
                // `tool_defs` 的 `inputSchema` 是声明面、本阶段不执行（`:1489` 那条理由原样适用——
                // 两处执行就有两种"不合法"的说法，而错误分型族只允许一个家）。
                //
                // 三者都是**可选**的，缺省值就是今天的行为：`intent=auto`、`depth=1`、
                // `include_edges=false` ⇒ 构造出的 `RecallOpts` 与 `RecallOpts::default()` 同值，
                // 库侧（`recall.rs` 的 `attach_layers` / `layer_readout`）在那一组里一次经历表、
                // 一次边表都不查，也不报 `no_episode`/`no_edges`。"默认逐字节不变"这一条由
                // `tests/recall_layers.rs` 的 `默认参数下_命中_l1_而没有_l2_不是错误_也不带邻域`
                // 与 BASE worktree 的逐字节 diff 两面钉住。
                //
                // 宽容读法与既有 `view`/`limit` 一族一致：**类型不对就当没给**（`as_str`/`as_i64`/
                // `as_bool` 读不出来就是缺省），只有"给了、但越界"才拒。
                let intent = match args.get("intent").and_then(Value::as_str).unwrap_or("auto") {
                    "auto" => rc_agent::recall::Intent::Auto,
                    "why" => rc_agent::recall::Intent::Why,
                    "when" => rc_agent::recall::Intent::When,
                    "who" => rc_agent::recall::Intent::Who,
                    "what" => rc_agent::recall::Intent::What,
                    // 不回显原串（守卫 12：读数里不许带调用方给的东西），闭集与 `bad_view` 同写法。
                    _ => return Err(rc_agent::ToolError::validation(
                        "bad_intent", "intent must be auto|why|when|who|what")),
                };
                let depth = args.get("depth").and_then(Value::as_i64).unwrap_or(1);
                if !(1..=2).contains(&depth) {
                    return Err(rc_agent::ToolError::validation(
                        "depth_out_of_range", "depth must be 1 or 2"));
                }
                let include_edges = args.get("include_edges").and_then(Value::as_bool).unwrap_or(false);
                // B4：闸拒分诊的**显式选择加入**。宽容读法与 `include_edges` 同族（非布尔按
                // 缺省 false 读）；缺省 ⇒ 返回里没有 `gate_deny_diag` 键，默认视图逐字节不变。
                let deny_diag = args.get("deny_diag").and_then(Value::as_bool).unwrap_or(false);
                if deny_diag && view != "json" {
                    // 诊断面只落在程序面（选型理由在报告 §3）：叙述面的字节有钉字用例，
                    // 且"悄悄忽略 opt-in"会让调用方误以为拿到了诊断。显式拒，不猜。
                    return Err(rc_agent::ToolError::validation(
                        "deny_diag_json_only", "deny_diag is only available with view=json"));
                }
                let opts = rc_agent::recall::RecallOpts {
                    intent, depth: depth as usize, include_edges, deny_diag,
                };
                // 三个确定性视图：不调模型、不需要 query/handles；limit 上限 50
                // （它们返回的是结构化清单，不是召回候选，20 那道帽不适用）。
                if matches!(view, "overview" | "agenda" | "timeline") {
                    let limit = args.get("limit").and_then(Value::as_i64).unwrap_or(20);
                    if !(1..=50).contains(&limit) {
                        return Err(rc_agent::ToolError::validation(
                            "limit_out_of_range", "view limit must be 1..=50"));
                    }
                    let viewer = self.agent_viewer();
                    let memory = self.root.memory().ok_or_else(no_memory)?;
                    let text = rc_agent::views::render(
                        &memory,
                        &rc_agent::views::ViewRequest {
                            view,
                            scope: args.get("scope").and_then(Value::as_str),
                            from_ms: args.get("from_ms").and_then(Value::as_i64),
                            to_ms: args.get("to_ms").and_then(Value::as_i64),
                            limit: limit as usize,
                        },
                        &viewer,
                        now,
                    )
                    .map_err(rc_agent::ToolError::from)?;
                    return Ok(self.read_view(text, now));
                }
                // 工具契约 §4：`limit` 默认 8、最大 20。这里只拒超限的那一档，
                // 召回本身的分页/排序一个字不改（"不改变现有召回逻辑"）。
                if args.get("limit").and_then(Value::as_i64).is_some_and(|n| n > 20) {
                    return Err(rc_agent::ToolError::validation("limit_too_large", "limit <= 20"));
                }
                // `card`/`expand` 只认句柄：按查询猜一条会把"我要这条"变成"我觉得像这条"。
                let wants_handles = matches!(view, "card" | "expand");
                let req = if wants_handles {
                    let h = args.get("handles").and_then(Value::as_array).ok_or_else(|| {
                        rc_agent::ToolError::validation(
                            "handles_required", "view=card|expand needs handles; a query is not enough")
                    })?;
                    let raw: Vec<String> = h.iter().filter_map(Value::as_str).map(String::from).collect();
                    if raw.is_empty() {
                        return Err(rc_agent::ToolError::validation(
                            "handles_required", "view=card|expand needs at least one handle"));
                    }
                    let (req, _dropped) = rc_agent::RecallRequest::handles(&raw)?;
                    req
                } else if let Some(q) = args.get("query").and_then(Value::as_str) {
                    rc_agent::RecallRequest::query(q)?
                } else if let Some(h) = args.get("handles").and_then(Value::as_array) {
                    let raw: Vec<String> = h.iter().filter_map(Value::as_str).map(String::from).collect();
                    let (req, _dropped) = rc_agent::RecallRequest::handles(&raw)?;
                    req
                } else {
                    return Err(rc_agent::ToolError::validation(
                        "empty_query",
                        "recollect_recall needs query or handles; got neither",
                    ));
                };
                // **过滤器**：中心面恒空（今天的形状不变）；节点面是**第一个非空构造点**。
                //
                // 节点面**只暴露 app / entity / time_window 三项**，**不暴露 device**：
                // 副本里的行带着中心的设备名，而节点不该按设备名筛——那会让节点
                // 以为"这台机器产生的内容"与"别的机器产生的内容"是两回事。
                // 少一项是**减暴露**，不是漏做。
                //
                // `time_basis`（C6）不是第四项过滤，是 `time_window` 的**选钟**：
                // `as_of`（缺省）按行的记录钟判，`event` 按 `time_json` 的事件时间判且**只留
                // 有事件时间的行**。给 `as_of` 与不给逐字同判；拼错的值显式拒，不静默折成缺省。
                let time_basis = match args.get("time_basis").and_then(Value::as_str) {
                    None => None,
                    Some(s) => Some(rc_agent::recall::TimeBasis::parse(s).ok_or_else(|| {
                        // 不回显原串（守卫 12：读数里不许带调用方给的东西），与 `bad_view` 同写法。
                        rc_agent::ToolError::validation(
                            "bad_time_basis", "time_basis must be as_of|event")
                    })?),
                };
                // `viewer`（S1b-C4a 节点面 / C4b1 中心面）：**中心与节点都过读闸**。
                // 读者身份是宿主自报的 agent 名（`initialize` 里那一格，与 `remember` 落的
                // `source_agent` 同源）：这位 agent 才看得见自己产的 private 行，跨 agent 的共享
                // 要经 `resource_grant`。W37-A1 起 viewer 还带启动配置声明的 principal：
                // 同 principal 异 agent 的行经读闸 4b 放行。中心面只是不加四项内容过滤
                // （`empty()` 的其余字段），不再拿 `None` 当"不过闸"——那会把中心面的越权读
                // 当成默认行为。中心 CLI 与测试仍可给 `None`（`RecallFilter::empty()` 就是那个形状）。
                let viewer = self.agent_viewer();
                let filter = if self.root.is_node() {
                    rc_agent::recall::RecallFilter {
                        device_id: None,
                        app: args.get("app").and_then(Value::as_str).map(String::from),
                        entity: args.get("entity").and_then(Value::as_str).map(String::from),
                        time_window: time_window_of(args),
                        time_basis,
                        viewer: Some(viewer),
                    }
                } else {
                    rc_agent::recall::RecallFilter {
                        viewer: Some(viewer),
                        ..rc_agent::recall::RecallFilter::empty()
                    }
                };
                let memory = self.root.memory().ok_or_else(no_memory)?;
                // 分派改走 I-5a 的 `recall_with` / `recall_narrative_with`：`opts` 是这一面唯一
                // 新增的输入，`view=json` 行内那七个新键（`layer`/`episode_id`/`mates`/`edges`/
                // `mode`/`basis`/`time_confidence`）**由库侧决定并算好**，这里只负责把它透出去，
                // 不在 mcp 侧另算一套（派发档 §1 最后一条）。旧函数 `recall`/`recall_narrative`
                // 原样转调 `RecallOpts::default()`，所以不传新参数的那一次调用与 BASE 同一条路。
                let text = match view {
                    "json" => rc_agent::recall::recall_with(&memory, &req, &filter, now, &opts)
                        .map(|j| j.text())?,
                    "brief" => rc_agent::recall::recall_narrative_with(
                        &memory, &req, &filter, now, rc_agent::NarrativeView::Brief, &opts,
                    ).map(|n| n.text())?,
                    "card" => rc_agent::recall::recall_narrative_with(
                        &memory, &req, &filter, now, rc_agent::NarrativeView::Card, &opts,
                    ).map(|n| n.text())?,
                    "expand" => rc_agent::recall::recall_narrative_with(
                        &memory, &req, &filter, now, rc_agent::NarrativeView::Expand, &opts,
                    ).map(|n| n.text())?,
                    // 上面已经拒过未知值；这里再拒一次而不是 `unreachable!`，
                    // 因为"闭集校验"和"渲染分派"一旦漂开，静默走到别的分支就是假话。
                    _ => return Err(rc_agent::ToolError::validation(
                        "bad_view", "view must be brief|json|card|expand")),
                };
                Ok(self.read_view(text, now))
            }
            "recollect_remember" => {
                let req = rc_agent::RememberRequest::new(args.get("body").and_then(Value::as_str).unwrap_or_default())?;
                // 缺省会话键由**已校验过的设备号**拼出来，所以它必然合法；
                // 不用宿主自报的 agent 名拼（那个可能是 `"Claude Code"`）。
                let key = args
                    .get("session_key")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .unwrap_or_else(|| format!("mcp:{}", self.device));
                // 第一个 `self` 是写缝、第二个是读缝；写缝 `append_agent_turn` 自己 `borrow_mut`
                // 的是 `agent`/`booted` 那两把独立 `RefCell`，而记忆句柄是直接的
                // 共享借用——撤 `RefCell<Root>`（任务 12 Step 3c）后这里再也不嵌套。
                //
                // 节点还没有副本时走 `remember_local`：转录层照写，只是没有可比的记忆库。
                match self.root.memory() {
                    Some(m) => rc_agent::tools::remember(
                        self, self, &m, Some(&self.agent_viewer()), &req, &key, now),
                    None => rc_agent::tools::remember_local(self, self, &req, &key, now),
                }
                .map(|j| j.text())
            }
            "recollect_vote" => {
                // 节点模式的记忆库是**只读副本**：表态要写进记忆库，而副本不是本机的账。
                // 响亮拒绝，不静默吞掉——"点了没反应"是最像成功的失败。
                if self.root.is_node() {
                    return Err(rc_agent::ToolError::unavailable(
                        "write_refused",
                        "the node holds a read-only replica; vote on the centre",
                    ));
                }
                let Some(stance) = rc_agent::VoteStance::from_slug(
                    args.get("stance").and_then(Value::as_str).unwrap_or_default(),
                ) else {
                    // `retired` **不再是 agent 面的合法值**（契约 §2.1/§3），所以它也不进这条
                    // hint 的可选值清单：hint 列了它，agent 就会一直试一个永远不成立的动作。
                    return Err(rc_agent::ToolError::validation(
                        "bad_stance",
                        "stance must be exactly one of confirmed|corroborated|rejected",
                    ));
                };
                // 契约 §3：agent 不能退休记忆（数据完整性），退休是 user/operator 的动作。
                // 形状先过 `from_slug`、再拒绝变体：这样 `retired` 与拼错的值分得开。
                if stance == rc_agent::VoteStance::Retired {
                    return Err(rc_agent::ToolError::validation(
                        "agent_cannot_retire",
                        "retirement is user/operator only",
                    ));
                }
                let memory = self.root.memory().ok_or_else(no_memory)?;
                // 读闸（S1b-C4b1）：表态的句柄校验过 agent 面读闸——被拒与"行不存在"同形
                // （`empty:row_absent`），存在性不外泄。中心面必过；节点面上面已经拒掉了。
                rc_agent::tools::vote_with_viewer(
                    &memory,
                    Some(&self.agent_viewer()),
                    args.get("target").and_then(Value::as_str).unwrap_or_default(),
                    stance,
                    args.get("anchor").and_then(Value::as_str),
                    args.get("reason").and_then(Value::as_str),
                    now,
                )
                .map(|j| j.text())
            }
            "recollect_status" => {
                let turn = args.get("turn").and_then(Value::as_str);
                let entity = args.get("entity").and_then(Value::as_str);
                // G19 刀 1：`view` 形参。闭集在这里拒（与 `recollect_recall` 的 `bad_view`
                // 同一条写法：schema 只是声明，本阶段不执行）；`None`/null = 缺省 Full，
                // 缺省面的行为一字不变。解析的家在 `StatusView::from_slug`（精确匹配）。
                let view = match args.get("view").and_then(Value::as_str) {
                    None => rc_agent::tools::StatusView::Full,
                    Some(raw) => rc_agent::tools::StatusView::from_slug(raw).ok_or_else(|| {
                        rc_agent::ToolError::validation("bad_view", "view must be full|counts")
                    })?,
                };
                // 缝与顶层既有格同一个 JSON 门面（后续增量只在其上增键）：`turn` 给了才有 `"extraction"` 那一格。
                // `entity` 是 P3 目录的过滤词（与 `turn` 在工具面互斥为 Validation）。
                let memory = self.root.memory().ok_or_else(no_memory)?;
                let text = rc_agent::tools::status(
                    &memory,
                    Some(self),
                    &rc_agent::tools::StatusOptions { turn, entity, view },
                    now,
                )
                .map(|j| j.text())?;
                Ok(self.read_view(text, now))
            }
            "recollect_export" => {
                // 契约 §3/§5：agent 面**不能导出全库**，导出是 user/operator 的动作。
                // 默认关，只有启动时显式 `--enable-export` 才放行；即便放行也有条数上限。
                if !self.allow_export {
                    return Err(rc_agent::ToolError::validation(
                        "export_disabled",
                        "export is user/operator only; restart with --enable-export",
                    ));
                }
                let limit = match args.get("limit") {
                    None | Some(Value::Null) => None,
                    Some(v) => {
                        let n = v.as_u64().ok_or_else(|| {
                            rc_agent::ToolError::validation("bad_limit", "limit must be a non-negative integer")
                        })?;
                        if n > 200 {
                            return Err(rc_agent::ToolError::validation("limit_too_large", "limit <= 200"));
                        }
                        Some(usize::try_from(n).unwrap_or(200))
                    }
                };
                // 读闸（S1b-C4b1）：导出是**人类面**，读者身份是宿主自报的 agent。
                // 被拒的行在导出件里不出现、不解释（`render_export` 里的 `facts_visible`），
                // 而 `limit` 与"还有 N 行未列出"两处读数都按可见集算。
                let req = rc_agent::ExportRequest {
                    now_ms: now,
                    limit,
                    viewer: Some(self.human_viewer()),
                };
                let memory = self.root.memory().ok_or_else(no_memory)?;
                let text = rc_agent::export::render_export(&memory, &req)
                    .map(|d| d.text())?;
                Ok(self.read_view(text, now))
            }
            "recollect_report" => {
                // 节点模式的记忆库是**只读副本**：报告要进中心的审核队列（与 vote 同一条理由）。
                if self.root.is_node() {
                    return Err(rc_agent::ToolError::unavailable(
                        "write_refused",
                        "the node holds a read-only replica; report on the centre",
                    ));
                }
                let target = required_str(args, "target")?;
                let category = required_str(args, "category")?;
                let note = args.get("note").and_then(Value::as_str);
                // 裸 turn_uid 目标的**存在性**在这一臂查（门面握着转录柄；守卫 6 不许
                // rc-agent 碰 rc-ledger）：形状合法但指不到轮的报告是悬空报告，
                // 与 `vote` 的"先认行再写"同一条纪律。行句柄目标的存在性与读闸
                // 由 `rc_agent::tools::report` 查（`row_absent`）。
                if rc_agent::tools::is_turn_uid(target) {
                    let known = rc_ledger::transcript::turn_origin(self.root.transcript(), target)
                        .map_err(store_down)?;
                    if known.is_none() {
                        return Err(rc_agent::ToolError::empty(
                            "turn_absent",
                            "the turn does not resolve to a transcript turn; nothing was queued",
                        ));
                    }
                }
                let memory = self.root.memory().ok_or_else(no_memory)?;
                let out = rc_agent::tools::report(
                    &memory,
                    Some(&self.agent_viewer()),
                    &self.caller_label(),
                    &self.device,
                    target,
                    category,
                    note,
                    now,
                )?;
                // W34 评审缺陷②的修法：报告项 enqueue 之后**补写该 pass 的审查轴水位**。
                // 不写的话 report 项 pending 期间实表 +1、水位 +0 ⇒ doctor 的
                // 「水位和==实表」必误报、`hub pending` 的 `review.pending` 漏数。
                // 无条件写（fresh=false 重报时重算快照同样正确，语义与 `hub review decide`
                // 同律）；写入口只有 `rc_distill::review::run::write_watermark` 这一处
                // （不本地复刻），best-effort：水位是读数，写失败不改变这次入队的结果。
                let _ = rc_distill::review::run::write_watermark(
                    &memory, self.root.ledger(), "report", now);
                Ok(out.text())
            }
            // ── 波二刀 A（G1 复用反馈）：`recollect_reuse` 的写入口。
            // **不复用 `recollect_vote`**：表态=我认为真假，复用=它帮没帮上忙，两轴语义不同，
            // 混进一个闭集就是把一格装两义。判定逻辑（闭集/句柄形状/读闸/写口）住
            // `rc_agent::tools::reuse_with_viewer`——与本文件其余记忆面工具同一条分工
            // （门面只管副本拒、参数读取与回执搬运，守卫 3 的同一个面）。
            "recollect_reuse" => {
                // 节点模式的记忆库是**只读副本**：反馈要写进记忆库（与 vote 同一条理由）。
                if self.root.is_node() {
                    return Err(rc_agent::ToolError::unavailable(
                        "write_refused",
                        "the node holds a read-only replica; report reuse on the centre",
                    ));
                }
                let handle = required_str(args, "handle")?;
                let outcome = required_str(args, "outcome")?;
                let note = args.get("note").and_then(Value::as_str);
                let memory = self.root.memory().ok_or_else(no_memory)?;
                rc_agent::tools::reuse_with_viewer(
                    &memory,
                    Some(&self.agent_viewer()),
                    handle,
                    outcome,
                    note,
                    &self.caller_label(),
                    now,
                )
                .map(|j| j.text())
            }
            // ── W38a 刀 B：转录检索面三件（细案 2026-11-08-w38a-transcript-search §三）。
            // 检索与信封的家在 rc-agent（本文件不出现 `rc_search`，P-47 同一条）；
            // viewer 照 `agent_viewer()` 的先例（--agent-id / --principal-id 启动项，W37 已铺）。
            //
            // W39 刀 D：节点面（`--replica`）**远端优先、本地回落**（细案 §3.2）：
            // 中心答 200 ⇒ 它的信封原样透传（已带 `source:"center"` 与 `epoch`）；
            // 未配对 / 不可达 / 被拒 ⇒ 本地读数并标 `source:"local"`（后两种再标 degraded）。
            // `Store::Center`（本机=中心）一字不动：仍然直接走本地实现、不加 source 键。
            "recollect_search" => {
                if self.root.is_node() {
                    // 远端只收已经过形状门的入参（与本地同一把尺）；形状不合法就交给
                    // 本地那一条给出原来的 validation 错误，并照旧留痕。
                    if let (Ok(query), Ok(limit)) = (
                        required_str(args, "query"),
                        transcript_limit(args, 20, "limit <= 20"),
                    ) {
                        match node::remote_transcript::search(
                            self.root.data_dir(),
                            query,
                            Some(limit),
                            &self.remote_transcript_extra(),
                        ) {
                            Ok(v) => return Ok(rc_agent::ToolJson::passthrough(v).text()),
                            Err(fault) => {
                                let j = self.local_search_readout(args, now)?;
                                return Ok(local_transcript(j, fault));
                            }
                        }
                    }
                }
                Ok(self.local_search_readout(args, now)?.text())
            }
            "recollect_session" => {
                let session_key = required_str(args, "session_key")?;
                let limit = transcript_limit(args, 200, "limit <= 200")?;
                let after_uid = args.get("after_uid").and_then(Value::as_str);
                if self.root.is_node() {
                    match node::remote_transcript::session(
                        self.root.data_dir(),
                        session_key,
                        after_uid,
                        Some(limit),
                        &self.remote_transcript_extra(),
                    ) {
                        Ok(v) => return Ok(rc_agent::ToolJson::passthrough(v).text()),
                        Err(fault) => {
                            let j = rc_agent::tools::session(
                                self.root.transcript(),
                                &self.agent_viewer(),
                                session_key,
                                limit,
                                after_uid,
                            )?;
                            return Ok(local_transcript(j, fault));
                        }
                    }
                }
                rc_agent::tools::session(
                    self.root.transcript(),
                    &self.agent_viewer(),
                    session_key,
                    limit,
                    after_uid,
                )
                .map(|j| j.text())
            }
            // ── W39 刀 B（细案 §3.4）：按轮取原文；形状门与信封的家都在 rc-agent。
            // 刀 D 起节点面远端优先，但**形状不合法的句柄不发远端**：中心只会回 400，
            // 那会把调用方的 bug 读成一次网络故障；本地实现给出原本的 `bad_turn_handle`。
            "recollect_turn" => {
                let turn_uid = required_str(args, "turn_uid")?;
                let before = window_side(args, "before", "before <= 10")?;
                let after = window_side(args, "after", "after <= 10")?;
                if self.root.is_node() && rc_agent::tools::is_turn_uid(turn_uid) {
                    match node::remote_transcript::turn(
                        self.root.data_dir(),
                        turn_uid,
                        before,
                        after,
                        &self.remote_transcript_extra(),
                    ) {
                        Ok(v) => return Ok(rc_agent::ToolJson::passthrough(v).text()),
                        Err(fault) => {
                            let j = rc_agent::tools::turn(
                                self.root.transcript(),
                                &self.agent_viewer(),
                                turn_uid,
                                before,
                                after,
                            )?;
                            return Ok(local_transcript(j, fault));
                        }
                    }
                }
                rc_agent::tools::turn(
                    self.root.transcript(),
                    &self.agent_viewer(),
                    turn_uid,
                    before,
                    after,
                )
                .map(|j| j.text())
            }
            "recollect_sessions" => {
                let limit = transcript_limit_opt(args, 200)?;
                if self.root.is_node() {
                    match node::remote_transcript::sessions(
                        self.root.data_dir(),
                        limit,
                        &self.remote_transcript_extra(),
                    ) {
                        Ok(v) => return Ok(rc_agent::ToolJson::passthrough(v).text()),
                        Err(fault) => {
                            let j = rc_agent::tools::sessions(
                                self.root.transcript(),
                                &self.agent_viewer(),
                                limit,
                            )?;
                            return Ok(local_transcript(j, fault));
                        }
                    }
                }
                rc_agent::tools::sessions(self.root.transcript(), &self.agent_viewer(), limit)
                    .map(|j| j.text())
            }
            // ── G19 刀 3：身份自查。四格里三格是运行时绑定（不走参数，契约 §1.1 同一条），
            // `primary_user` 来自 core 载荷——core_brief 必填的 `user_id` 由此可发现。
            "recollect_whoami" => {
                // 主语闸与 core_* 同一把：caller 为空/"unknown" 时，"你是谁"的诚实答案是
                // "先把主语定下来"，而不是把 "unknown" 当身份发出去。
                let agent = self.core_agent()?;
                // core 打不开（密钥箱坏了那一族）⇒ 响亮拒绝：这层答不了就说是答不了，
                // 不把"未注册"当兜底——两者的下一步动作不同（doctor vs register）。
                let core = self.core.as_ref().ok_or_else(|| {
                    rc_agent::ToolError::unavailable("core_unavailable", "run: doctor")
                })?;
                let (registered, primary_user) = match core.load_doc(&agent) {
                    Ok(payload) => (
                        true,
                        payload.and_then(|p| p.subjects.into_iter().find(|s| s.is_primary))
                            .map(|s| json!({
                                "user_id": s.subject_id,
                                "profile_text": s.profile_text,
                            })),
                    ),
                    Err(rc_core::CoreError::Unregistered) => (false, None),
                    Err(e) => return Err(core_error(e)),
                };
                let mut out = json!({
                    "agent_id": agent,
                    "device_id": self.device.as_str(),
                    "principal_id": self.principal.as_deref(),
                    "registered": registered,
                    "primary_user": primary_user,
                });
                // 未注册是**读数**不是故障（isError=false）：hint 是可粘贴的命令模板，
                // `<agent_id>`/`<dir>` 是占位符——零用户字节回显的纪律照旧。
                if !registered {
                    out.as_object_mut().expect("json! 的字面量是对象").insert(
                        "register_hint".to_string(),
                        json!(REGISTER_HINT_CMD),
                    );
                }
                Ok(out.to_string())
            }
            // ── G19 刀 4：会话两阶段自查。分型与键名住 rc-agent（`ingest_status`），
            // 读数经缝进来；`no_rows` 按码翻成 isError=false（`is_recall_empty_readout`
            // 的按码判据，谁发都一样）。
            "recollect_ingest_status" => {
                let session_key = required_str(args, "session_key")?;
                rc_agent::tools::ingest_status(self, session_key).map(|j| j.text())
            }
            _ => Err(rc_agent::ToolError::validation("unknown_tool", "not in capabilities; check tools/list")),
        }
    }

    /// 本次调用的 core 主语：CLI 优先，其次 `clientInfo.name` 兜底（契约 §1.1）。
    fn caller_label(&self) -> String {
        match &self.cli_agent {
            Some(agent) => agent.clone(),
            None => self.agent.borrow().clone(),
        }
    }

    /// core 工具的统一身份闸。空与 `"unknown"` 都是"没有登记过的 caller"：
    /// 它们进不了写路径（否则 `source_agent`/`device_id` 会被记到一个不存在的人头上）。
    fn core_agent(&self) -> Result<String, rc_agent::ToolError> {
        let agent = self.caller_label();
        if agent.is_empty() || agent == "unknown" {
            return Err(rc_agent::ToolError::validation(
                "unregistered",
                "restart mcp with --agent-id",
            ));
        }
        Ok(agent)
    }

    /// `core_access_log` 的唯一写点。**detail 只写短语**（错误码/参数名），不写正文、
    /// 不写密文、不写 key；写失败不影响回执（账是账，答案是答案）。
    fn log_core(&self, caller: &str, tool: &str, result: &str, detail: Option<&str>, now: i64) {
        if let Some(store) = &self.core {
            let caller = if caller.is_empty() { "unknown" } else { caller };
            let _ = store.log_access(caller, tool, None, result, detail, now);
        }
    }

    /// `core_*` 的唯一分发点。顺序是**身份 → 库 → 工具**：没身份的人不该先知道 core.db 在不在。
    /// 每次调用都写 `core_access_log`（ok/denied/not_found），detail 只落错误码。
    fn call_core(&self, name: &str, args: &Value, now: i64) -> Result<String, rc_agent::ToolError> {
        let caller = self.caller_label();
        let agent = match self.core_agent() {
            Ok(agent) => agent,
            Err(e) => {
                self.log_core(&caller, name, "denied", Some(e.code), now);
                return Err(e);
            }
        };
        let Some(store) = self.core.as_ref() else {
            // core.db 打不开不阻止进程启动，但这一层必须响亮自报（契约 §7 的 `unavailable`）。
            return Err(rc_agent::ToolError::unavailable("core_unavailable", "run: doctor"));
        };
        // core.db 治理小刀①（2026-11-07 分诊）：这份库里没有这个 caller 的注册行时，
        // 分派口先**自动注册**（幂等）再执行——消"未注册撞墙"。注册只建表与注册行、
        // 不产内容，是低摩擦不是代写；判定用 `doc_version`（注册面最便宜的读法）。
        // 边界：这只在 core_* 分派口发生。hook 是只读注入面，**不**在 hook 路径加注册写；
        // `recollect_whoami` 是诊断面，也不注册——它的"未注册"读数必须如实。
        if matches!(store.doc_version(&agent), Err(rc_core::CoreError::Unregistered)) {
            store.register_agent(&agent, now).map_err(core_error)?;
        }
        let outcome = core_call(store, &agent, &self.device, name, args, now);
        match &outcome {
            Ok(_) => self.log_core(&agent, name, "ok", None, now),
            Err(e) if e.code == "not_found" => {
                self.log_core(&agent, name, "not_found", Some(e.code), now)
            }
            Err(e) => self.log_core(&agent, name, "denied", Some(e.code), now),
        }
        outcome
    }

    /// 节点模式下那一份副本的"截至时刻"。**中心模式下没有这一格**——
    /// 中心手里的数据就是当下的，不存在滞后。
    fn replica_as_of(&self) -> Option<i64> {
        let state = node::state::load(&node::state::state_path(self.root.data_dir())).ok().flatten()?;
        if state.current_manifest.epoch == 0 && state.current_manifest.seq == 0 {
            return None;
        }
        Some(state.current_manifest.created_at_ms)
    }

    /// 节点模式的读面自报出处与截至时刻。**中心模式的返回逐字不变**。
    /// 工具回执是 JSON 时加三格；`export` 是 Markdown，就在文首加一行注释。
    fn read_view(&self, text: String, now: i64) -> String {
        if !self.root.is_node() {
            return text;
        }
        let staging = node::staging::is_active(self.root.data_dir());
        let source = if staging { "pending_reconcile" } else { "replica" };
        let as_of = self.replica_as_of();
        let lag = as_of.map(|ms| now - ms);
        let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
            let stamp = as_of.map(|ms| ms.to_string()).unwrap_or_else(|| "unknown".into());
            return format!("<!-- source: {source}; as_of: {stamp} -->\n\n{text}");
        };
        if let Some(obj) = v.as_object_mut() {
            obj.insert("source".to_string(), json!(source));
            obj.insert("as_of".to_string(), json!(as_of));
            obj.insert("lag_ms".to_string(), json!(lag));
            if staging {
                let pending = node::staging::new_rows(self.root.data_dir());
                if let Some(rows) = obj.get_mut("rows").and_then(Value::as_array_mut) {
                    for row in rows.iter_mut() {
                        let hit = row
                            .get("id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| pending.iter().any(|p| p == id));
                        if hit {
                            if let Some(o) = row.as_object_mut() {
                                o.insert("pending".to_string(), json!(true));
                            }
                        }
                    }
                }
            }
        }
        v.to_string()
    }

    /// P1 的附着规则：本进程第一次也是唯一一次。**返回 true 就是"这次带上"**。
    fn take_bootstrap(&self) -> bool {
        let mut b = self.booted.borrow_mut();
        let first = !*b;
        *b = true;
        first
    }

    /// 两行制（`agent_tool` 主语 + `device` 主语）。§9.5 要同时回答"哪个工具在被用"与
    /// "哪台设备在用"，而 `usage_counter` 的冲突键含 `subject_kind` ⇒ 两行不打架。
    /// 计数失败**不许影响回执**（回执已经算了，计数只是账），所以这里忽略错误而不是上报：
    /// 上报它意味着给一条已经生成好的消息新增一种失败方式，而那正是"读数为了一句话把答案弄丢"。
    fn bump(&self, tool: &str, now: i64) {
        // 裁定 R106：`agent_tool` 的主语集合就是 `capabilities().tools`，清单外的名字**两行都不打**。
        // 计数器表不许为不存在的工具建立正读数——幻觉名做主语会把 `doctor --capabilities` 的键名
        // 换成模型可控文本（那是 carry 清单里"路径形状进 `usage_counter` 列"那一半的另一条路），
        // 而"宿主拼错了名字"这件事本回合已经由 `unknown_tool` 回执响亮报出，跳过计数不丢读数。
        // 判据是**成员资格**不是形状白名单：`safe_token` 管的是形状，这里要的是"在清单内"。
        if !rc_agent::capabilities().tools.contains(&tool) {
            return;
        }
        // 撤掉 `RefCell<Root>` 后（任务 12 Step 3c），这里直接走访问器；R106 那两条
        // "计数失败不影响回执"靠 `let _ =` 保留，与借用无关。
        let _ = rc_ledger::counters::bump(self.root.ledger(), "agent_tool", tool, "tool_call", 1, now);
        let _ = rc_ledger::counters::bump(self.root.ledger(), "device", &self.device, "tool_call", 1, now);
    }
}

/// 副本不在时的唯一错误形状。**码与提示都是常量**。
fn no_memory() -> rc_agent::ToolError {
    rc_agent::ToolError::unavailable("no_memory", "no snapshot on this node yet — run: node sync")
}

/// **`no_rows` 是正常状态，不是失败**（Task 10）：`agent-onboarding` §1 把 `no_rows`
/// （已有副本/库是好的，这次查询没命中）与 `no_memory`（本机还没有可用副本）分成两格，
/// 接入文档 §2 还专门写了"空召回不等于丢数据"。而 `isError` 正是宿主判断"这次调用失败了吗"
/// 的那一格：把它标成 `true`，一次沉默就被读成一次故障（并按 transport 问题重发成洪水）。
///
/// **只翻 `isError` 这一维**：文本仍是 `ToolError` 的 `kind:code hint` 三段原样，
/// 机读码 `no_rows` 一字不动（它是错误目录的键）。这里也不改 `rc_agent` 的返回形状——
/// 空读数分型的家是 `rc-agent/recall.rs` 的 `empty_readout`（`recall` 与 `recall_narrative`
/// 两个调用点共用它，`recall_identity_silence.rs` 逐格钉着 `(kind, code)`），
/// `mcp` 只是它的一个消费者 ⇒ 在本边界翻一维，分型的家一字不动。
///
/// 判据按**码**收窄，不放宽成"所有 `Empty` 都不算 error"：`row_absent` / `turn_absent` /
/// `not_found` 说的是"指认了一条不存在的东西"（调用方的动作错了），`no_memory` 说的是
/// "这台还没有可用副本"（运维状态，接入文档 §5 要求报给操作员）——那两族仍走 `isError: true`。
/// `kind == Empty` 一起判：码是那一格，kind 是它所属的那一型，两者同真才是这里。
///
/// **这层契约是说破的**：判据按**码**建立、不按工具建立 ⇒ 将来别的工具若也发 `no_rows`，
/// 会**静默继承**这一维（不标 `isError`）——那是有意的：`no_rows` 这一码在设计里就是
/// "有库、这次没命中"的正常读数，谁发都一样，不是某个工具的专属豁免。
///
/// **Phase 4（I-5b）加的正是这一格的另一半**。库侧（`recall.rs` 的 `layer_readout`）多了两个
/// 同族读数：`no_episode`（"命中了行，但它们都不属于任何经历"）与 `no_edges`（"命中了行，
/// 但没有一条可见的关系边触达它们"）。它们在分型上仍是 `Empty`——服务正常、输入正常、
/// 库里那一层没有——而 `Empty` 一路的默认是 `is_error=true`。**照 BASE 那一句不改，
/// "这次没有 episode"就会变成一次工具故障**：宿主把它读成 transport 问题，于是按 §
/// 那条"空召回不等于丢数据"的接入文档重发成洪水。
///
/// 为什么这里只列三码、不写成"所有 `Empty` 都不标 error"（派发档 §2-A 的命门）：
/// 同一型里 `row_absent` / `turn_absent` / `not_found` 说的是**指认了一条不存在的东西**
/// （调用方的动作错了，下一步是改句柄），`visible_none_under_identity` 是 `Authorization`
/// 型（换身份才是下一步）。把它们一起放过去，"沉默"与"你用错了"就又混成一格了。
/// 用例：`recall_layers.rs` 的 `no_episode_与_no_edges_是正常读数_is_error_为_false`
/// 与 `row_absent_仍是错误_判据没放宽成所有_empty`，四码可分那一格还在
/// `replica_mode.rs` 的 `空读数四码可分` 里把 `no_memory` 一起对上账。
fn is_recall_empty_readout(e: &rc_agent::ToolError) -> bool {
    e.kind == rc_agent::AgentErrorKind::Empty
        && matches!(e.code, "no_rows" | "no_episode" | "no_edges")
}

/// `time_window` 的解析：`{"from": i64, "to": i64}`，闭区间。
/// 两端缺一 ⇒ 不筛（不是筛一个空区间）——空区间会**筛掉所有行**，
/// 那是最像"库里没有"的一种失败。
fn time_window_of(args: &Value) -> Option<(i64, i64)> {
    let w = args.get("time_window")?;
    let from = w.get("from").and_then(Value::as_i64)?;
    let to = w.get("to").and_then(Value::as_i64)?;
    if from > to {
        return None;
    }
    Some((from, to))
}

/// 交接条 `include` 的唯一解析点：缺省/`null` ⇒ `None`（调用方用各自的缺省），
/// 必须是字符串数组且元素 ∈ `HANDOFF_KINDS`；拼错不猜成"全都要"。
fn handoff_include(args: &Value) -> Result<Option<Vec<String>>, rc_agent::ToolError> {
    match args.get("include") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let items = value.as_array().ok_or_else(|| {
                rc_agent::ToolError::validation(
                    "bad_kind",
                    "include takes continue|due|reminder|note",
                )
            })?;
            let mut kinds = Vec::with_capacity(items.len());
            for item in items {
                let kind = item
                    .as_str()
                    .filter(|k| rc_core::HANDOFF_KINDS.iter().any(|known| known == k))
                    .ok_or_else(|| {
                        rc_agent::ToolError::validation(
                            "bad_kind",
                            "include takes continue|due|reminder|note",
                        )
                    })?;
                kinds.push(kind.to_string());
            }
            Ok(Some(kinds))
        }
    }
}

/// `core_notebook_append` 的 tags 解析：缺省/`null` ⇒ 空，必须是字符串数组。
/// 元素级形状（非空、条数、每条长度）的家在 rc-core 的 `notebook_append`——错误分型族
/// 只允许一个家，这里不重复那几把尺。
fn notebook_tags(args: &Value) -> Result<Vec<String>, rc_agent::ToolError> {
    match args.get("tags") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(String::from).ok_or_else(|| {
                    rc_agent::ToolError::validation("bad_tags", "tags must be an array of strings")
                })
            })
            .collect(),
        Some(_) => Err(rc_agent::ToolError::validation(
            "bad_tags",
            "tags must be an array of strings",
        )),
    }
}

/// `core_notebook_list`/`core_notebook_search` 共用的 limit：缺省/`null` ⇒ `None`
/// （缺省值 50/20 住 rc-core）；>200 就地拒，不静默截成上限（同 `core_handoff_list` 口径）。
fn notebook_limit_arg(args: &Value) -> Result<Option<usize>, rc_agent::ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value.as_u64().ok_or_else(|| {
                rc_agent::ToolError::validation("bad_limit", "limit must be a non-negative integer")
            })?;
            if n > 200 {
                return Err(rc_agent::ToolError::validation("bad_limit", "limit <= 200"));
            }
            Ok(Some(usize::try_from(n).unwrap_or(200)))
        }
    }
}

/// list/search 的零正文投影（细案 §3：回执**没有 body 键**，`NotebookEntry` 也确实没有——
/// 这里手工拼是为了让"零正文"在这一个函数里肉眼可见，而不是依赖 serde 的字段巧合）。
fn notebook_notes_json(notes: &[rc_core::ops::NotebookEntry]) -> Vec<Value> {
    notes
        .iter()
        .map(|n| {
            json!({
                "id": n.id,
                "title": n.title,
                "tags": n.tags,
                "created_at_ms": n.created_at_ms,
            })
        })
        .collect()
}

/// `expires_at_ms` 的唯一拒绝形状：**整数**毫秒，别的形状不猜。
/// 静默回落成"没给"会把它变成 7 天默认——一条本该今天到期的提醒就再也不在窗里出现。
fn bad_expires() -> rc_agent::ToolError {
    rc_agent::ToolError::validation(
        "bad_expires",
        "expires_at_ms must be an integer epoch in milliseconds",
    )
}

/// `core_*` 的语义层（工具契约 §3/§6）。**只转调 `rc_core`**：这里不碰密文、不拼表名、
/// 不自己写 payload（payload 的演进住在 rc-core 的密文里）。`device_id` 取本进程的设备主语，
/// `now` 取工具面这一刻——两者都不从参数进来。
fn core_call(
    store: &rc_core::CoreStore,
    agent: &str,
    device: &str,
    name: &str,
    args: &Value,
    now: i64,
) -> Result<String, rc_agent::ToolError> {
    match name {
        // ---- 读：简报 ----
        "core_brief" => {
            let user_id = args.get("user_id").and_then(Value::as_str).unwrap_or_default();
            if user_id.trim().is_empty() {
                return Err(rc_agent::ToolError::validation(
                    "empty_user",
                    "core_brief needs the user_id it should render for",
                ));
            }
            let scene = args.get("scene").and_then(Value::as_str).unwrap_or_default();
            let budget = core_budget(args)?;
            let session_key = args.get("session_key").and_then(Value::as_str);
            // `include` 是交接段的种类过滤器：缺省走 `DEFAULT_INCLUDE`（接着做 + 到期）。
            let parsed = handoff_include(args)?;
            let include: Vec<&str> = match &parsed {
                Some(kinds) => kinds.iter().map(String::as_str).collect(),
                None => rc_core::DEFAULT_INCLUDE.to_vec(),
            };
            // 渲染 + `core_injection_log` 都在 rc-core 里（分区预算与淘汰只有一个家）。
            let brief = store
                .brief_with(agent, user_id, scene, budget, session_key, &include, now)
                .map_err(core_error)?;
            Ok(brief.text)
        }
        // ---- 开关：中文表达优化 ----
        "core_chinese_expression_get" => {
            let payload = core_payload(store, agent)?;
            Ok(pretty(&json!({
                "enabled": payload.chinese_expression.enabled,
                "version": payload.chinese_expression.version,
                "updated_at_ms": payload.chinese_expression.updated_at_ms.to_string(),
                "rules": rc_core::CHINESE_EXPRESSION_RULES,
            })))
        }
        "core_chinese_expression_set" => {
            let enabled = args.get("enabled").and_then(Value::as_bool).ok_or_else(|| {
                rc_agent::ToolError::validation("bad_enabled", "enabled must be true or false")
            })?;
            let version = store
                .chinese_expression_set(agent, enabled, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        // ---- 读：文档里的五段 ----
        "core_persona_get" => {
            let payload = core_payload(store, agent)?;
            Ok(pretty(&json!({
                "persona": payload.persona,
                "proposals": payload.persona_proposals,
            })))
        }
        "core_subject_get" => {
            let payload = core_payload(store, agent)?;
            Ok(pretty(&json!({"subjects": payload.subjects})))
        }
        "core_interaction_get" => {
            let payload = core_payload(store, agent)?;
            // 关系与交互同属 §9.1 的"关系 + 交互状态"分区，一起给才回答得了
            // "你有多久没理我了"（§9.5）。
            Ok(pretty(&json!({
                "interaction": payload.interaction,
                "relationship": payload.relationship,
            })))
        }
        "core_affect_get" => {
            let payload = core_payload(store, agent)?;
            Ok(pretty(&json!({"affects": payload.affects})))
        }
        "core_loop_list" => {
            let want = args.get("status").and_then(Value::as_str);
            if let Some(status) = want {
                if !matches!(status, rc_core::LOOP_OPEN | rc_core::LOOP_DONE | rc_core::LOOP_DROPPED) {
                    return Err(rc_agent::ToolError::validation(
                        "bad_status",
                        "status must be open|done|dropped",
                    ));
                }
            }
            let payload = core_payload(store, agent)?;
            let loops: Vec<rc_core::LoopItem> = payload
                .loops
                .into_iter()
                .filter(|item| want.is_none_or(|status| item.status == status))
                .collect();
            Ok(pretty(&json!({"loops": loops})))
        }
        "core_episode_expand" => {
            let episode_id = required_str(args, "episode_id")?;
            let payload = core_payload(store, agent)?;
            let Some(pinned) = payload
                .pinned_episodes
                .iter()
                .find(|p| p.episode_id == episode_id)
            else {
                // "没有这条 pin"是 Empty，不是错误输入：句柄形状没问题，只是指不到东西。
                return Err(rc_agent::ToolError::empty(
                    "not_found",
                    "the episode is not pinned for this agent",
                ));
            };
            // 只回指针与理由：经历正文在普通记忆里，展开受那边的 scope 权限约束（契约 §6）。
            Ok(pretty(&json!({
                "episode_id": &pinned.episode_id,
                "pin_reason": &pinned.pin_reason,
            })))
        }
        // ---- 写：人格提案 / 审批 / 回滚 ----
        "core_persona_propose" => {
            // `PersonaPatch` 没有 `l0` 且 `deny_unknown_fields`：带 `l0` 的 patch 在这里
            // 就失败（L0 不可自改，契约 §6）。
            let patch: rc_core::PersonaPatch =
                serde_json::from_value(args.get("patch").cloned().unwrap_or(Value::Null)).map_err(|_| {
                    rc_agent::ToolError::validation(
                        "bad_patch",
                        "patch takes the six L1 persona fields only; l0 is not editable",
                    )
                })?;
            let reason = required_str(args, "reason")?;
            let evidence = match args.get("evidence") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(items)) => {
                    items.iter().filter_map(|v| v.as_str().map(String::from)).collect()
                }
                Some(_) => {
                    return Err(rc_agent::ToolError::validation(
                        "bad_evidence",
                        "evidence must be an array of strings",
                    ))
                }
            };
            let (version, proposal_id) = store
                .persona_propose(agent, patch, reason, evidence, device, now)
                .map_err(core_error)?;
            Ok(pretty(&json!({"version": version, "proposal_id": proposal_id})))
        }
        "core_persona_apply" | "core_persona_reject" => {
            let proposal_id = required_str(args, "proposal_id")?;
            let version = if name == "core_persona_apply" {
                store.persona_apply(agent, proposal_id, device, now)
            } else {
                store.persona_reject(agent, proposal_id, device, now)
            }
            .map_err(core_error)?;
            Ok(version_json(version))
        }
        "core_persona_revert" => {
            let version = core_version_arg(args)?;
            let version = store
                .persona_revert(agent, version, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        // ---- 写：主用户 / 交互 / 情绪 / 未闭环 / pinned ----
        "core_subject_update" => {
            let subject: rc_core::Subject =
                serde_json::from_value(args.get("subject").cloned().unwrap_or(Value::Null)).map_err(|_| {
                    rc_agent::ToolError::validation(
                        "bad_subject",
                        "subject must be a core subject object; see tools/list",
                    )
                })?;
            let version = store
                .subject_update(agent, subject, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        "core_interaction_touch" => {
            let touch: rc_core::InteractionTouch =
                serde_json::from_value(args.get("touch").cloned().unwrap_or(Value::Null)).map_err(|_| {
                    rc_agent::ToolError::validation(
                        "bad_touch",
                        "touch takes timestamps, counters and a topic only",
                    )
                })?;
            let version = store
                .interaction_touch(agent, touch, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        "core_affect_update" => {
            let affect: rc_core::Affect =
                serde_json::from_value(args.get("affect").cloned().unwrap_or(Value::Null)).map_err(|_| {
                    rc_agent::ToolError::validation(
                        "bad_affect",
                        "affect must carry subject_kind, label and confidence; see tools/list",
                    )
                })?;
            let version = store
                .affect_update(agent, affect, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        // 治理小刀③：relationship 写口（payload 11 段里唯一零写者的那一段）。形状照
        // core_affect_update：整段替换、校验的家在 rc-core（四个比值字段 0..=1），
        // 工具面只透传；身份/设备/时钟由 `call_core` 绑定，schema 无 agent_id。
        "core_relationship_update" => {
            let relationship: rc_core::Relationship =
                serde_json::from_value(args.get("relationship").cloned().unwrap_or(Value::Null))
                    .map_err(|_| {
                        rc_agent::ToolError::validation(
                            "bad_relationship",
                            "relationship must be an object with trust, intimacy, tension, \
                             status, text and confidence; see tools/list",
                        )
                    })?;
            let version = store
                .relationship_update(agent, relationship, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        "core_loop_upsert" => {
            let item: rc_core::LoopItem =
                serde_json::from_value(args.get("item").cloned().unwrap_or(Value::Null)).map_err(|_| {
                    rc_agent::ToolError::validation(
                        "bad_loop",
                        "item must be a loop object with id, kind, text and status",
                    )
                })?;
            let version = store
                .loop_upsert(agent, item, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        "core_loop_close" => {
            let id = required_str(args, "id")?;
            let status = required_str(args, "status")?;
            let version = store
                .loop_close(agent, id, status, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        "core_episode_pin" => {
            let episode_id = required_str(args, "episode_id")?;
            let reason = required_str(args, "reason")?;
            let version = store
                .episode_pin(agent, episode_id, reason, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        "core_episode_unpin" => {
            let episode_id = required_str(args, "episode_id")?;
            let version = store
                .episode_unpin(agent, episode_id, device, now)
                .map_err(core_error)?;
            Ok(version_json(version))
        }
        // ---- 写/读：交接条（一次性便签，不进抽取/召回/快照） ----
        "core_handoff_write" => {
            let kind = required_str(args, "kind")?;
            let text = required_str(args, "text")?;
            // 缺省有效期由 rc-core 给（`now + 7d`）；工具面只透传调用方给的那一格。
            // 给了就必须是**整数**：字符串/浮点/布尔一律拒，不静默回落成 7 天默认
            // （"明天"被当成没给会让一条本该到期的提醒变成 7 天后才到）。
            let expires_at_ms = match args.get("expires_at_ms") {
                None | Some(Value::Null) => None,
                Some(Value::Number(number)) => Some(number.as_i64().ok_or_else(bad_expires)?),
                Some(_) => return Err(bad_expires()),
            };
            let written = store
                .handoff_write(agent, kind, text, expires_at_ms, device, now)
                .map_err(core_error)?;
            Ok(pretty(&json!({
                "id": written.id,
                "superseded": written.superseded,
                "version": written.version,
            })))
        }
        "core_handoff_list" => {
            // `include` 是 kind 的过滤器；元素必须落在闭集里，拼错不猜成"全都要"。
            let include = handoff_include(args)?;
            let limit = match args.get("limit") {
                None | Some(Value::Null) => 50,
                Some(value) => {
                    let n = value.as_u64().ok_or_else(|| {
                        rc_agent::ToolError::validation(
                            "bad_limit",
                            "limit must be a non-negative integer",
                        )
                    })?;
                    if n > 200 {
                        return Err(rc_agent::ToolError::validation("bad_limit", "limit <= 200"));
                    }
                    usize::try_from(n).unwrap_or(200)
                }
            };
            let mut handoffs = store.handoff_list(agent, now).map_err(core_error)?;
            if let Some(kinds) = include {
                handoffs.retain(|h| kinds.iter().any(|kind| kind == &h.kind));
            }
            handoffs.truncate(limit);
            // `expired` 由工具面标：rc-core 照常返回"未关闭但已过期"的条（写路径给它们留了
            // 7 天宽限），这里补一格让调用方看得见"到点了"，而不是靠它自己算时间。
            let items: Vec<Value> = handoffs
                .iter()
                .map(|h| {
                    json!({
                        "id": h.id,
                        "kind": h.kind,
                        "text": h.text,
                        "created_at_ms": h.created_at_ms,
                        "expires_at_ms": h.expires_at_ms,
                        "seen_count": h.seen_count,
                        "last_seen_at_ms": h.last_seen_at_ms,
                        "closed_at_ms": h.closed_at_ms,
                        "closed_reason": h.closed_reason,
                        "expired": h.expires_at_ms <= now,
                    })
                })
                .collect();
            Ok(pretty(&json!({"handoffs": items})))
        }
        "core_handoff_close" => {
            let id = required_str(args, "id")?;
            let reason = args.get("reason").and_then(Value::as_str);
            let closed = store
                .handoff_close(agent, id, reason, device, now)
                .map_err(core_error)?;
            Ok(pretty(&json!({
                "id": closed.id,
                "closed_at_ms": closed.closed_at_ms,
                "reason": closed.reason,
                "already_closed": closed.already_closed,
                "version": closed.version,
            })))
        }
        // ---- 写/读：小本本（W36 细案 §3：per-agent 加密动态表，append-only） ----
        // 身份与 device/now 都由 `call_core` 绑定；任何臂都不收 `agent_id`（契约 §1.1，
        // 在 `call_tool` 入口就被拒）。形状校验的家在 rc-core（`notebook_append`），
        // 这里只做"参数形状对不对得起透传"的解析：数组不是字符串数组、limit 不是整数，就地拒。
        "core_notebook_append" => {
            let body = required_str(args, "body")?;
            let title = args.get("title").and_then(Value::as_str);
            let tags = notebook_tags(args)?;
            let source_turn_uid = args.get("source_turn_uid").and_then(Value::as_str);
            let written = store
                .notebook_append(agent, title, &tags, body, source_turn_uid, device, now)
                .map_err(core_error)?;
            Ok(pretty(&json!({
                "id": written.id,
                "created_at_ms": written.created_at_ms,
            })))
        }
        "core_notebook_list" => {
            // limit 缺省/0 由 rc-core 折成 50；>200 在这里就拒，不静默截成上限（同 handoff 口径）。
            let limit = notebook_limit_arg(args)?;
            let tag = args.get("tag").and_then(Value::as_str);
            let listed = store.notebook_list(agent, limit, tag).map_err(core_error)?;
            Ok(pretty(&json!({
                "notes": notebook_notes_json(&listed.notes),
                "unlisted": listed.unlisted,
            })))
        }
        "core_notebook_search" => {
            // 多词是「或」，解密后线性匹配——这一切都住 rc-core（不上 FTS5 是已拍板裁定：
            // 给密文上 FTS5 需要明文列或盲索引，破「core.db 无明文」）。空命中是合法空读数
            // （isError=false 的空 notes），不是错误。
            let query = required_str(args, "query")?;
            let limit = notebook_limit_arg(args)?;
            let found = store.notebook_search(agent, query, limit).map_err(core_error)?;
            Ok(pretty(&json!({
                "notes": notebook_notes_json(&found.notes),
                "unlisted": found.unlisted,
            })))
        }
        "core_notebook_expand" => {
            let id = required_str(args, "id")?;
            let full = store.notebook_expand(agent, id).map_err(core_error)?;
            Ok(pretty(&json!({
                "id": full.id,
                "title": full.title,
                "tags": full.tags,
                "body": full.body,
                "source_turn_uid": full.source_turn_uid,
                "created_at_ms": full.created_at_ms,
            })))
        }
        _ => Err(rc_agent::ToolError::validation(
            "unknown_tool",
            "not in capabilities; check tools/list",
        )),
    }
}

/// 当前文档；还没写过就是空载荷（"注册了但还没有内容"不是错误，契约 §3）。
fn core_payload(
    store: &rc_core::CoreStore,
    agent: &str,
) -> Result<rc_core::CorePayload, rc_agent::ToolError> {
    Ok(store
        .load_doc(agent)
        .map_err(core_error)?
        .unwrap_or_else(|| rc_core::CorePayload::empty(agent)))
}

/// `core_brief` 的预算：缺省 20000；**超过 20000 直接拒**，不静默截成上限
/// （"截断后假装成功"是把调用方的 `over_budget` 折成一个看起来正常的简报）。
fn core_budget(args: &Value) -> Result<usize, rc_agent::ToolError> {
    match args.get("budget") {
        None | Some(Value::Null) => Ok(rc_core::DEFAULT_BUDGET),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                rc_agent::ToolError::validation("bad_budget", "budget must be a non-negative integer")
            })?;
            if n > rc_core::DEFAULT_BUDGET as u64 {
                return Err(rc_agent::ToolError::validation("over_budget", "budget <= 20000"));
            }
            Ok(usize::try_from(n).unwrap_or(rc_core::DEFAULT_BUDGET))
        }
    }
}

fn core_version_arg(args: &Value) -> Result<u32, rc_agent::ToolError> {
    args.get("version")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            rc_agent::ToolError::validation("bad_version", "version must be a non-negative integer")
        })
}

fn version_json(version: u32) -> String {
    pretty(&json!({"version": version}))
}

/// 必填字符串参数。空/纯空白按"没给"处理：空白句柄不猜成一条行，理由空白不猜成一次提案。
fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, rc_agent::ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            rc_agent::ToolError::validation(
                "missing_argument",
                "a required argument is missing or blank; see tools/list",
            )
        })
}

/// `recollect_search` / `recollect_session` 的 limit 形参解析：缺省/`null` ⇒ `0`
/// （`0` = 用库层缺省，缺省值住 rc-agent/rc-search，这里不抄第二份）；越界就地拒，
/// 不静默截成上限（与 `core_notebook_list` 口径同族）。hint 按调用点给，保持在
/// `'static`（错误文本零拼接的纪律）。
fn transcript_limit(
    args: &Value,
    max: usize,
    too_large_hint: &'static str,
) -> Result<usize, rc_agent::ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(0),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                rc_agent::ToolError::validation("bad_limit", "limit must be a non-negative integer")
            })?;
            if n > max as u64 {
                return Err(rc_agent::ToolError::validation("bad_limit", too_large_hint));
            }
            Ok(usize::try_from(n).unwrap_or(max))
        }
    }
}

/// `recollect_sessions` 的可选 limit：缺省/`null` ⇒ 全量；同一把越界尺。
fn transcript_limit_opt(
    args: &Value,
    max: usize,
) -> Result<Option<usize>, rc_agent::ToolError> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(None),
        Some(_) => transcript_limit(args, max, "limit <= 200").map(Some),
    }
}

/// `recollect_turn` 的 `before` / `after`：缺省/`null` ⇒ `0`（只要目标轮）；范围 `0..=10`，
/// 越界就地拒，不静默截成上限（与 [`transcript_limit`] 同一把尺）。上限的家在
/// `rc-search` 的 `MAX_WINDOW_SIDE`，`rc-agent` 的工具层会再钳一次——这里只是让越界
/// 变成一条可读的 validation 错误，而不是被库层悄悄折成 10。
fn window_side(
    args: &Value,
    key: &'static str,
    too_large_hint: &'static str,
) -> Result<usize, rc_agent::ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(0),
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                rc_agent::ToolError::validation(
                    "bad_window",
                    "before/after must be non-negative integers <= 10",
                )
            })?;
            if n > 10 {
                return Err(rc_agent::ToolError::validation("bad_window", too_large_hint));
            }
            Ok(usize::try_from(n).unwrap_or(10))
        }
    }
}

fn pretty(v: &Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

/// `unregistered` 的**可粘贴命令串**（Task 9 的裁定形状：`--data-dir` 是 `recollect-core`
/// 的父级参数、必须排在 `register` 之前，顺序反了 clap 在解析期直接拒）。两个读者共用
/// 这一个常量：`recollect_whoami` 的 `register_hint` 字段（G19 刀 3），以及 core_error
/// 那条 hint 的命令段——后者因 `ToolError::hint` 是 `&'static str` 整串无法拼接而保留
/// 原字面量，两处的漂移由 `tests/g19_selfserve.rs` 的 whoami 用例与
/// `core_tools.rs::unregistered_hint_points_at_the_core_registry_entry` 两头钉住。
const REGISTER_HINT_CMD: &str = "recollect-core --data-dir <dir> register <agent_id>";

/// `CoreError` → `ToolError` 的唯一映射（本文件内）。错误文本只给类别与下一步：
/// 载荷（`BadPayload` 的说明）不进 hint——它可能带着调用方刚写下的字段名。
fn core_error(e: rc_core::CoreError) -> rc_agent::ToolError {
    use rc_core::CoreError as CE;
    match e {
        // 未注册 ≠ 没传 `--agent-id`：走到这里调用方**已经**带了主语（没带的话在
        // `core_agent` 那道闸就被"restart mcp with --agent-id"拦下了，见 :751）。
        // 所以下一步是**注册**，不是重启一次再做同一个动作。占位符 `<agent_id>`/`<dir>`
        // 是命令模板，不是回显——hint 里零用户字节这条纪律不变。
        // 参数顺序是被执法的那一格（`tests/core_tools.rs` 的顺序断言 +
        // `rc-core/tests/cli_data_dir_order.rs` 的真 CLI 正反）：`--data-dir` 是
        // `recollect-core` 的**父级**参数（`rc-core/src/bin/recollect-core.rs:20-24`，
        // clap v4、没有 `global = true`），必须写在子命令**前**；写在后面会被解析期直接拒
        // （`unexpected argument` / 退出码 2），连数据目录都不建 ⇒ 照抄即失败。
        CE::Unregistered => rc_agent::ToolError::validation(
            "unregistered",
            "no core registration for this agent; run: recollect-core --data-dir <dir> register <agent_id>",
        ),
        CE::NotFound => rc_agent::ToolError::empty(
            "not_found",
            "the target does not exist in this agent's core",
        ),
        CE::BadPayload(_) => rc_agent::ToolError::validation(
            "bad_payload",
            "the request does not fit the core schema; see tools/list",
        ),
        // 这一臂与 `Unregistered` **不同 hint**：id 连合法形状都没有，`recollect-core register`
        // 自己在 `checked_agent_id` 那一层就会拒掉它——把同一个"去注册"塞给这条臂，
        // 指的是一个永远做不成的动作。分型判据本来就是"人下一步做什么不同"（`rc-agent/error.rs` 档头）。
        // 注：本门面里 `--agent-id` 在启动期已过 `safe_token`（非法 ⇒ 退 3），`clientInfo.name`
        // 兜底不过闸时被写成 `"unknown"` 并被 `core_agent` 拦下，所以这条臂在 mcp 面上**测不到**，
        // 它是映射穷尽性要求的防御支。
        CE::BadAgentId => rc_agent::ToolError::validation(
            "unregistered",
            "the agent id is not a safe token; run: doctor",
        ),
        // 满了要说下一步做什么：先关掉或用等过期；条数只在 `CoreError` 的正文里，不进 hint。
        CE::HandoffFull(_) => rc_agent::ToolError::validation(
            "handoff_full",
            "close or let expire some handoff notes first",
        ),
        // 密钥/密文/表名/IO/SQL 五族对调用方是同一个下一步：先跑 doctor。
        CE::Crypto | CE::BadTableName | CE::BadKeyFile | CE::KeyMissing => {
            rc_agent::ToolError::unavailable("core_unavailable", "run: doctor")
        }
        CE::Io(_) | CE::Sql(_) => rc_agent::ToolError::unavailable("core_unavailable", "run: doctor"),
    }
}

/// `tools/list` 的形状 = `capabilities().tools` 的清单 × 这里的定义表。
/// 清单来自 `rc_agent`（唯一来源），描述与 schema 住在这里（它们只有线上这一个读者）。
/// 两者数量不吻合时返回 `None`，由 `main` 拒绝启动。
fn tool_defs() -> Option<Vec<Value>> {
    let mut out = Vec::new();
    for name in rc_agent::capabilities().tools {
        out.push(json!({"name": name, "title": title(name)?, "description": description(name)?,
                        "inputSchema": input_schema(name)?}));
    }
    Some(out)
}

fn title(name: &str) -> Option<&'static str> {
    Some(match name {
        "recollect_recall" => "查记忆",
        "recollect_remember" => "记一条",
        "recollect_vote" => "表态",
        "recollect_status" => "状态",
        "recollect_export" => "导出",
        "recollect_report" => "报告有误",
        "recollect_reuse" => "复用反馈",
        "recollect_search" => "转录检索",
        "recollect_session" => "会话回放",
        "recollect_turn" => "按轮取原文",
        "recollect_sessions" => "会话清单",
        "recollect_whoami" => "我是谁",
        "recollect_ingest_status" => "导入进度",
        "core_brief" => "核心简报",
        "core_chinese_expression_get" => "中文表达读取",
        "core_chinese_expression_set" => "中文表达开关",
        "core_persona_get" => "人格读取",
        "core_persona_propose" => "人格提案",
        "core_persona_apply" => "人格应用",
        "core_persona_revert" => "人格回滚",
        "core_persona_reject" => "人格拒绝",
        "core_subject_get" => "主用户读取",
        "core_subject_update" => "主用户更新",
        "core_interaction_get" => "交互读取",
        "core_interaction_touch" => "交互记录",
        "core_affect_get" => "情绪读取",
        "core_affect_update" => "情绪更新",
        "core_relationship_update" => "关系更新",
        "core_loop_list" => "未闭环列表",
        "core_loop_upsert" => "未闭环登记",
        "core_loop_close" => "未闭环关闭",
        "core_episode_pin" => "经历钉住",
        "core_episode_unpin" => "经历取消钉住",
        "core_episode_expand" => "经历展开",
        "core_handoff_write" => "交接条写入",
        "core_handoff_list" => "交接条列表",
        "core_handoff_close" => "交接条关闭",
        "core_notebook_append" => "小本本记一条",
        "core_notebook_list" => "小本本列表",
        "core_notebook_search" => "小本本检索",
        "core_notebook_expand" => "小本本展开",
        _ => return None,
    })
}

/// 描述文本是给 agent 读的（C2 的"教它什么时候该查"那一半在工具面上，另一住在 L0 skill 里）。
fn description(name: &str) -> Option<&'static str> {
    Some(match name {
        "recollect_recall" => "Look up memory. Default view=brief returns a natural-language brief with [n] handles; view=json returns rows for programs; view=card returns full cards for handles; view=expand returns one memory's evidence chain. view=overview|agenda|timeline are deterministic structured views (no query needed; optional scope / from_ms / to_ms / limit<=50). To ask about the layer above the row itself, pass intent=when (the episode this belongs to, plus its neighbours) or intent=why (the visible relationship edges touching it); include_edges=true asks for the same edges on any view — the brief and card text then carries an L2/L3 block, view=json carries the episode_id / mates / edges keys. Empty at that layer is a normal readout, not a failure: no_episode and no_edges come back with isError=false, exactly like no_rows. Archived (value-demoted) rows are omitted by default from the query arm and from the three structured views, and the omission is counted (value_mix.archived on view=json / brief's omission line; facts_hidden_by_value on the views); explicit addressing (handles, view=card|expand) still returns them, because the archive never deletes anything. depth=2 widens the edge walk one more hop (1 or 2 only). time_basis picks the clock for time_window on the node face: as_of (default) filters by the row's recorded time, event filters by the row's event time from its parsed time expression and keeps only rows that have one — rows without an event time never match under event.",
        "recollect_remember" => "Append one agent turn to the transcript. Extraction happens later, centrally.",
        "recollect_vote" => "Confirm, corroborate or reject a memory row by handle. Retirement is user/operator only.",
        "recollect_status" => "Diagnostic counters for this library: rows per table (anchor/card), open rejections/retirements/expired/cooling_down, dangling attestations, attention_open, recheck_pending, index state, distill lag with last extract time, freshness ages, quota/cost cells, per-scope lag and halted scopes. No bodies, no memory content. Optional turn=<64-hex turn_uid> adds that turn's extraction readout under extraction (a ghost handle is turn_absent with isError=false); optional entity=<name or alias> adds the entity directory filtered to it (mutually exclusive with turn). The default view is the full object and also carries the tool-capability declaration; view=counts returns the counters only — no capability declaration and no entity directory (entity and view=counts are mutually exclusive too).",
        "recollect_whoami" => "Identity readout for this agent face, no arguments: agent_id, device_id, principal_id (null when the operator did not declare one) and primary_user — the primary subject's user_id and profile_text from core.db; pass that user_id to core_brief. registered=false means this agent has no core registration: the readout carries a pasteable register_hint and nothing is broken (isError=false). registered=true with primary_user=null means registered but no subject profile yet (core_subject_update writes one).",
        "recollect_ingest_status" => "Two-phase ingest readout for one session_key: turns_total (extractable turns, role != system), turns_distilled (those with a terminal extraction pass row), turns_pending, and last_arrival_ms (newest ingested_at_ms, all roles). Import makes turns immediately searchable via recollect_search / recollect_session; distilled turns surface in recall — this tool tells you which phase the session is in instead of guessing. A session with no turns is no_rows (isError=false), not a failure.",
        "recollect_export" => "Human-readable Markdown export of the memory library. User/operator only; off by default.",
        "recollect_report" => "Report a memory row (handle '<table>:<64 hex>') or a transcript turn (bare 64-hex turn_uid) as wrong. Takes a short category tag and an optional note. The report goes into the review queue for human judgement; nothing is changed or superseded automatically. Reporting the same target with the same content twice is deduplicated (fresh=false).",
        // 波二刀 A：复用反馈。描述要教"什么时候该报"（onboarding 文案在刀 B）。
        "recollect_reuse" => "Report whether a recalled card actually helped you. handle is a card handle ('card:<64 hex>'); outcome is exactly one of worked (the card answered the question), failed (it did not help), partial (partly), mismatch (the card's conditions did not hold), stale (it is no longer true). Optional note is a short phrase. This is a different axis from recollect_vote: vote is about truth, reuse is about usefulness, so neither replaces the other. Append-only: reporting the same card with the same outcome twice records two events, which is the point (worked twice reads differently from worked once). The outcome is consumed on the read side: worked >= 2 lifts the card in ranking (a single worked does not), stale makes recall treat it as stale (silent), and failed / mismatch additionally enter the human review queue under source='reuse' for re-examination. Nothing in the card is changed and no body is echoed back.",
        "recollect_search" => "Search the transcript layer for verbatim turns, independent of recall (it never touches memory.db and does not consume the recall injection budget). query is required; limit is 1..=20 (default 10). Each hit carries turn_uid, session_key, role, created_at_ms and a snippet; the envelope carries hits, matched (the pre-gate match count), truncated, and coverage {indexed, total} (indexed far below total means a rebuild is due). Empty readouts are normal (isError=false) and distinguishable by matched: hits empty with matched=0 means nothing indexed matches; matched>0 with no hits means the matching turns are all outside your visibility gate (only your own agent's sessions and same-principal sessions are ever searched).",
        "recollect_session" => "Replay one transcript session's turns verbatim (full bodies) — the playback companion to recollect_search, under the same visibility gate: a session you cannot see is indistinguishable from one that does not exist (no_rows, isError=false). session_key is required; limit is 1..=200 (default 200); optional after_uid is a cursor that returns strictly the turns after that turn — a cursor outside the session yields no_rows, never a rewind.",
        "recollect_turn" => "Fetch one transcript turn verbatim by its 64-lowercase-hex turn_uid, together with its neighbours inside the same session: before and after (each 0..=10, default 0) say how many turns to include on either side. The envelope returns turns in ascending order with the target among them, each carrying turn_uid, role, body and created_at_ms, alongside session_key, device_id, owner_agent_id, owner_principal_id and truncated_before / truncated_after (true when that side still had more turns than requested). Same visibility gate as recollect_search and recollect_session: a turn that does not exist and a turn you may not see both come back as no_rows with isError=false, so existence is never leaked. turn_uid is required; a malformed handle (not 64 lowercase hex) is a validation error.",
        "recollect_sessions" => "List the transcript sessions visible to you (same gate as recollect_search): session_key, source_agent, turn count, first/last timestamps and project_hint — the browsing companion to recollect_session. Independent of recall. Optional limit caps the list and reports truncated=true when it did.",
        "core_brief" => "The resident 20K brief for this agent: persona, primary user, interaction, affect, loops, pinned episodes. Includes open handoff notes (kind continue|due by default; pass include to change) so a new session can pick up where the last one stopped; notes are not memory and never appear in recall.",
        "core_chinese_expression_get" => "Read the Chinese-expression optimization switch and its fixed rules.",
        "core_chinese_expression_set" => "Turn the Chinese-expression optimization switch on or off. Rules are fixed; there is nothing to rewrite.",
        "core_persona_get" => "Read this agent's L1 persona and its self-edit proposals.",
        "core_persona_propose" => "Propose an L1 persona patch with a reason. Applying it is a separate step.",
        "core_persona_apply" => "Apply a proposed persona patch; produces a new version.",
        "core_persona_revert" => "Revert the persona to a past version; history is never deleted.",
        "core_persona_reject" => "Reject a proposed persona patch without touching the persona.",
        "core_subject_get" => "Read the primary user and core-relationship profiles.",
        "core_subject_update" => "Upsert one subject profile (1 primary + at most 3 core relations).",
        "core_interaction_get" => "Read last user message, last agent initiative, unanswered count and the current topic.",
        "core_interaction_touch" => "Record timestamps, counters or the current topic. No free-form bodies.",
        "core_affect_get" => "Read the current user/agent affect (label, valence, confidence, validity).",
        "core_affect_update" => "Replace one affect row (user or agent) with confidence and validity.",
        "core_relationship_update" => "Replace this agent's relationship readout with the user — the agent's own stance, written by the agent itself (never derived by programs). One object, whole-segment replace: trust, intimacy, tension and confidence are ratios in 0..=1; status is a short free-form word (e.g. normal) and text is a one-line description. Shows up in the brief's relationship section and in core_interaction_get; out-of-range values are refused and nothing is written.",
        "core_loop_list" => "List open loops (promises, conflicts, topics), optionally filtered by status.",
        "core_loop_upsert" => "Upsert one open loop by id.",
        "core_loop_close" => "Close a loop as done or dropped.",
        "core_episode_pin" => "Pin a shared episode by pointer, with a reason.",
        "core_episode_unpin" => "Unpin an episode pointer; idempotent.",
        "core_episode_expand" => "Expand a pinned episode pointer into its episode_id and pin_reason.",
        "core_handoff_write" => "Write a one-shot handoff note for the next session (what to pick up, what is due, a reminder). Not a long-term task — use core_loop_* for recurring work. kind: continue|due|reminder|note; text <= 200 chars; expires_at_ms optional (default now + 7 days). A new continue supersedes the previous open one.",
        "core_handoff_list" => "List this agent's open handoff notes (including ones past their expiry, flagged expired=true), newest first within kind. include filters by kind (continue|due|reminder|note).",
        "core_handoff_close" => "Close a handoff note so it stops appearing. reason: done|superseded|manual (default manual). Closing twice is a no-op.",
        "core_notebook_append" => "Append a private notebook entry for this agent (encrypted at rest, append-only; nothing can be edited or deleted). Takes body (required, <= 2000 characters), optional title, optional tags (at most 8, each <= 32 chars) and optional source_turn_uid pointer. The notebook is not memory: it never enters recall or extraction.",
        "core_notebook_list" => "List this agent's notebook entries, newest first. Zero body fields: each row carries id, title, tags and created_at_ms only. Optional tag filters by one tag; limit defaults to 50 (<= 200). unlisted counts the entries that matched the filter but did not make the limit.",
        "core_notebook_search" => "Search this agent's notebook by substring across title, tags and body. Words are OR-ed; matching happens after decryption (the notebook is encrypted at rest), so this is a linear scan, not a full-text index. limit defaults to 20 (<= 200). Empty hits are a normal readout, not a failure.",
        "core_notebook_expand" => "Fetch one notebook entry in full (body byte-for-byte) by id (an 'n-' handle from append/list/search).",
        _ => return None,
    })
}

/// 本阶段**不做 JSON Schema 校验**：各函数各自校验自己的输入，schema 在这里是声明而不是执行。
/// 为什么不两处都执行：两处执行就有两种"不合法"的说法，而错误分型族只允许一个家（守卫 3）。
fn input_schema(name: &str) -> Option<Value> {
    Some(match name {
        // Phase 4（I-5b）只加不减：三个新参数在这里是**声明**（宿主拿来提示模型用的），
        // 执行点仍然只有 `call_tool` 那一处（见上面那段注释）。
        //
        // **登记一处既有漂移（非本轮引入，本轮不修）**：实现还读 `app` / `entity` /
        // `time_window`（节点面的三个过滤项，`call_tool` 的 `RecallFilter` 那一支），
        // 而这一份 schema 没有声明它们；`scope` / `from_ms` / `to_ms` 又只对三个确定性视图有效
        // （`view=brief|json|card|expand` 时它们不参与任何判定）。两句都是 BASE 上就有的形状，
        // 修它要连带决定"节点面到底暴露哪几项"，那不属于这一刀。
        "recollect_recall" => json!({"type":"object","properties":{"query":{"type":"string"},"handles":{"type":"array","items":{"type":"string"}},"limit":{"type":"integer"},"scope":{"type":"string"},"from_ms":{"type":"integer"},"to_ms":{"type":"integer"},"view":{"type":"string","enum":["brief","json","card","expand","overview","agenda","timeline"]},"intent":{"type":"string","enum":["auto","why","when","who","what"]},"depth":{"type":"integer","minimum":1,"maximum":2},"include_edges":{"type":"boolean"},"deny_diag":{"type":"boolean"},"time_basis":{"type":"string","enum":["as_of","event"]}}}),
        "recollect_remember" => json!({"type":"object","required":["body"],"properties":{"body":{"type":"string"},"session_key":{"type":"string"}}}),
        "recollect_vote" => json!({"type":"object","required":["target","stance"],"properties":{"target":{"type":"string"},"stance":{"type":"string"},"anchor":{"type":"string"},"reason":{"type":"string"}}}),
        // G19 刀 1：`view` 在 schema 里是声明（执行点在 `call_tool` 的 `bad_view` 那一支）。
        "recollect_status" => json!({"type":"object","properties":{
            "turn":{"type":"string"},"entity":{"type":"string"},
            "view":{"type":"string","enum":["full","counts"]}}}),
        "recollect_export" => json!({"type":"object","properties":{"limit":{"type":"integer"}}}),
        "recollect_report" => json!({"type":"object","required":["target","category"],"properties":{
            "target":{"type":"string"},"category":{"type":"string"},"note":{"type":"string"}}}),
        // 波二刀 A：schema 是声明，执行点在 `call_tool`（闭集与句柄形状都在那一处拒）。
        "recollect_reuse" => json!({"type":"object","required":["handle","outcome"],"properties":{
            "handle":{"type":"string"},
            "outcome":{"type":"string","enum":["worked","failed","partial","mismatch","stale"]},
            "note":{"type":"string"}}}),
        // W38a 转录检索面三件。schema 只是声明，执行点在 `call_tool` / rc-agent（见文件头注）。
        "recollect_search" => json!({"type":"object","required":["query"],"properties":{
            "query":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":20}}}),
        "recollect_session" => json!({"type":"object","required":["session_key"],"properties":{
            "session_key":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":200},
            "after_uid":{"type":"string"}}}),
        // W39 刀 B：schema 只是声明，形状门与范围在工具面（`rc-agent` 的 `is_turn_uid`
        // 与 `call_tool` 的 `window_side`）。
        "recollect_turn" => json!({"type":"object","required":["turn_uid"],"properties":{
            "turn_uid":{"type":"string"},
            "before":{"type":"integer","minimum":0,"maximum":10},
            "after":{"type":"integer","minimum":0,"maximum":10}}}),
        "recollect_sessions" => json!({"type":"object","properties":{
            "limit":{"type":"integer","minimum":1,"maximum":200}}}),
        // G19 自查两件。whoami 零参数（身份由运行时绑定）；ingest_status 只收会话键。
        "recollect_whoami" => json!({"type":"object","properties":{}}),
        "recollect_ingest_status" => json!({"type":"object","required":["session_key"],"properties":{
            "session_key":{"type":"string"}}}),
        // 契约 §1.1：**任何** schema 里都没有 `agent_id`——它们由运行时绑定，参数给了就是 denied。
        "core_brief" => json!({"type":"object","required":["user_id"],"properties":{
            "user_id":{"type":"string"},"scene":{"type":"string"},
            "budget":{"type":"integer"},"session_key":{"type":"string"},
            "include":{"type":"array","items":{"type":"string","enum":["continue","due","reminder","note"]}}}}),
        "core_chinese_expression_get" => json!({"type":"object","properties":{}}),
        "core_chinese_expression_set" => json!({"type":"object","required":["enabled"],"properties":{
            "enabled":{"type":"boolean"}}}),
        "core_persona_get" => json!({"type":"object","properties":{}}),
        "core_persona_propose" => json!({"type":"object","required":["patch","reason"],"properties":{
            "patch":{"type":"object"},"reason":{"type":"string"},
            "evidence":{"type":"array","items":{"type":"string"}}}}),
        "core_persona_apply" => json!({"type":"object","required":["proposal_id"],"properties":{
            "proposal_id":{"type":"string"}}}),
        "core_persona_reject" => json!({"type":"object","required":["proposal_id"],"properties":{
            "proposal_id":{"type":"string"}}}),
        "core_persona_revert" => json!({"type":"object","required":["version"],"properties":{
            "version":{"type":"integer"}}}),
        "core_subject_get" => json!({"type":"object","properties":{}}),
        "core_subject_update" => json!({"type":"object","required":["subject"],"properties":{
            "subject":{"type":"object"}}}),
        "core_interaction_get" => json!({"type":"object","properties":{}}),
        "core_interaction_touch" => json!({"type":"object","required":["touch"],"properties":{
            "touch":{"type":"object"}}}),
        "core_affect_get" => json!({"type":"object","properties":{}}),
        "core_affect_update" => json!({"type":"object","required":["affect"],"properties":{
            "affect":{"type":"object"}}}),
        "core_relationship_update" => json!({"type":"object","required":["relationship"],"properties":{
            "relationship":{"type":"object"}}}),
        "core_loop_list" => json!({"type":"object","properties":{"status":{"type":"string"}}}),
        "core_loop_upsert" => json!({"type":"object","required":["item"],"properties":{
            "item":{"type":"object"}}}),
        "core_loop_close" => json!({"type":"object","required":["id","status"],"properties":{
            "id":{"type":"string"},"status":{"type":"string"}}}),
        "core_episode_pin" => json!({"type":"object","required":["episode_id","reason"],"properties":{
            "episode_id":{"type":"string"},"reason":{"type":"string"}}}),
        "core_episode_unpin" => json!({"type":"object","required":["episode_id"],"properties":{
            "episode_id":{"type":"string"}}}),
        "core_episode_expand" => json!({"type":"object","required":["episode_id"],"properties":{
            "episode_id":{"type":"string"}}}),
        "core_handoff_write" => json!({"type":"object","required":["kind","text"],"properties":{
            "kind":{"type":"string","enum":["continue","due","reminder","note"]},
            "text":{"type":"string"},"expires_at_ms":{"type":"integer"}}}),
        "core_handoff_list" => json!({"type":"object","properties":{
            "include":{"type":"array","items":{"type":"string","enum":["continue","due","reminder","note"]}},
            "limit":{"type":"integer"}}}),
        "core_handoff_close" => json!({"type":"object","required":["id"],"properties":{
            "id":{"type":"string"},"reason":{"type":"string","enum":["done","superseded","manual"]}}}),
        // W36 小本本四件。契约 §1.1 同样适用：schema 里没有 `agent_id`，给了即 denied。
        // `tags`/`title`/`source_turn_uid` 的元素级形状（条数、每条长度）由 rc-core 执行，
        // 这里只声明形状（本阶段不做 JSON Schema 校验，见文件头注释）。
        "core_notebook_append" => json!({"type":"object","required":["body"],"properties":{
            "body":{"type":"string"},"title":{"type":"string"},
            "tags":{"type":"array","items":{"type":"string"}},
            "source_turn_uid":{"type":"string"}}}),
        "core_notebook_list" => json!({"type":"object","properties":{
            "limit":{"type":"integer"},"tag":{"type":"string"}}}),
        "core_notebook_search" => json!({"type":"object","required":["query"],"properties":{
            "query":{"type":"string"},"limit":{"type":"integer"}}}),
        "core_notebook_expand" => json!({"type":"object","required":["id"],"properties":{
            "id":{"type":"string"}}}),
        _ => return None,
    })
}
