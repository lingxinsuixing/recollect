use clap::{Args, Parser, Subcommand};
use rc_agent::surface::{probe_block, probe_via_search};
use rc_memory::{bump_cooldown, COOLDOWN_MS};
use rc_store::AgentRoot;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "recollect-hook", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// L1 状态块：SessionStart 用，零正文，一行 JSON
    Brief(BriefArgs),
    /// 三态闸门结果：UserPromptSubmit 用，沉默即零字节
    Probe(ProbeArgs),
}

#[derive(Args)]
struct BriefArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// core 载荷身份（W34 续接包）。给了才拼固定段「上一棒交接」：`core_handoff` 未闭条目
    /// + 项目状态读数。不给就整段缺席——brief 的既有形状（一行围栏 JSON）一字不动。
    #[arg(long)]
    agent_id: Option<String>,
    /// 判定用的时钟。缺省时读本机时钟——V14：纪律的落点是"库函数收 `now_ms` 形参"，
    /// 不是"全仓只有一个取时钟的地方"。阶段 5 的跨架构对拍**必须**传这个参数，
    /// 否则同一份快照在两台机器上因为差几毫秒给出不同结果，对拍会假红。
    #[arg(long)]
    now_ms: Option<i64>,
    /// 下行副本（阶段 4 的落地文件形状）。给了它就走只读记忆柄，`--data-dir` 只用于报形状。
    /// 本阶段它唯一的消费者是 `probe_cli.rs` 最后那条用例——但"副本上仍出声、冷却一格不盖"
    /// 只能在 CLI 层证，所以参数必须在 CLI 上。
    #[arg(long)]
    replica: Option<PathBuf>,
}

#[derive(Args)]
struct ProbeArgs {
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,
    /// probe 这一次读的读者身份（W37-A1）。给了它，probe 的候选集先过读闸——
    /// 读者看不见的行不进候选；构造的是**无 principal 的 agent viewer**，读闸对它
    /// 保持 fail-closed（4b 永不命中）。不给 ⇒ 不过闸（与改动前逐位相同的旧形状）。
    /// 过 `safe_token`：非法 ⇒ 退 2（与 brief 的 `--agent-id` 同一把尺子）。
    #[arg(long)]
    agent_id: Option<String>,
    /// 查询文本。缺省时从 stdin 整块读。
    /// **宿主不接 stdin 的配置必须用 `--query`**：没有管道时 `read_to_string` 会挂住，
    /// 而一个挂住的 hook 会把整场宿主卡住——这是 C2 之外唯一能让系统表现为"死"的形状。
    #[arg(long)]
    query: Option<String>,
    #[arg(long)]
    now_ms: Option<i64>,
    #[arg(long)]
    replica: Option<PathBuf>,
    /// 会话身份（V4 分片的键）。给了才写 `<data-dir>/inject/session-<sha256>.ids`；
    /// 不给就**零文件 I/O**（不建目录）。形状只在 rc-store 里校验：空串是调用方错误。
    #[arg(long)]
    session: Option<String>,
}

// 退出码即契约，分型沿用阶段 0（hub `main` 上方那行"退出码即契约"的注释——按内容点名，
// 不写行号：那一侧每插一次注释这边的引用就腐烂一次，而 R78 已把行号引用判为零读者形状）：
//   0 = 出声或沉默，两者都算成功；2 = 输入不可用；3 = 运维失败。
// **2 的唯一例外**（任务 5，本机即中心）：宿主 stdin **读成功**而里面没有词 ⇒ 0 + 零字节。
// 理由不在 CLI 侧而在宿主侧：Qoder 对 `UserPromptSubmit` 的契约是 exit 2 = 阻断这条 prompt，
// 而"附件-only / slash 命令 / resume"灌进来的正是空 prompt——**挡住用户说话不在记忆系统的权限里**，
// 而设计 §8.2 约束 3 的立场是"超时即沉默、宿主不支持时降级为沉默"。沉默的形状与
// "命中但全被闸门挡掉"逐字节相同（V3：零字节），所以这次放宽不新增任何一种可观测状态。
// 例外**只此一支**：`--query` 显式给空白、以及 `read_stdin()` 读不出来（R77 那条"读不出来 ≠
// 没有输入"）都仍是 2——判别必须带"输入来源"这一层，只看错误码会把那条运维信号一起吞掉。
// **1 本 target 永不返回**：hub 那边它是"逐条拒"，而 hook 只有一个输入，没有"逐条"可言。
// 把这句话写死在代码里是因为不写，下一个人会顺手给空查询发 1，那正好把 2 的含义偷走。
fn main() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        // help / version 与用法错误同 hub 分诊：clap 给用法错误定的码是 2，
        // 而 2 在这里专指"输入不可用"，敲错参数不该让操作员以为提示词是空的。
        Err(e) if e.use_stderr() => {
            return operational("invalid invocation — usage: recollect-hook brief|probe [--data-dir DIR] [--now-ms MS] [--replica FILE]; probe 另有 --query TEXT [--session KEY]")
        }
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(0);
        }
    };
    match cli.cmd {
        Cmd::Brief(a) => run_brief(&a),
        Cmd::Probe(a) => run_probe(&a),
    }
}

fn operational(msg: &str) -> ExitCode {
    // 类别名而已：StoreError / io::Error 的原串可能带路径，零路径规则不许外传
    // （同 hub 的 `operational`——按函数名点名，行号会腐烂）。
    use std::io::Write;
    let _ = std::io::stderr().write_all(format!("error: {msg}\n").as_bytes());
    ExitCode::from(3)
}

/// stderr 一行诊断。**不改判定、不改输出、不进 agent 面**。
fn note(msg: &str) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(format!("note: {msg}\n").as_bytes());
}

/// 工具面的错误原样落 stderr：`ToolError` 的 `Display` 是 `kind:code hint` 三段，
/// 而这三段全是本仓库的常量串——所以零正文、零输入回显在这里是**结构性质**，不是审查成果。
fn emit_error(e: &rc_agent::ToolError, code: u8) -> ExitCode {
    use std::io::Write;
    let _ = std::io::stderr().write_all(format!("error: {e}\n").as_bytes());
    ExitCode::from(code)
}

fn clock(given: Option<i64>) -> Option<i64> {
    given.or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|d| d.as_millis() as i64)
    })
}

fn open_root(data_dir: &Path, replica: Option<&Path>) -> Result<AgentRoot, rc_store::StoreError> {
    match replica {
        Some(snap) => AgentRoot::open_replica(data_dir, snap),
        None => AgentRoot::open(data_dir),
    }
}

/// **沉默 ⇔ 零字节**（V3）。只有非沉默才补结尾换行；沉默时一个字节都不写，
/// 因为"空的一行"与"没有输出"在宿主解析里不是同一件事，而 C2 承诺的是后者。
fn emit(block: &rc_agent::InjectionBlock) -> Result<(), ()> {
    use std::io::Write;
    if block.is_empty() {
        return Ok(());
    }
    let mut bytes = block.text().into_bytes();
    bytes.push(b'\n');
    let out = std::io::stdout();
    let mut lock = out.lock();
    lock.write_all(&bytes).and_then(|_| lock.flush()).map_err(|_| ())
}

fn run_brief(a: &BriefArgs) -> ExitCode {
    let Some(now) = clock(a.now_ms) else {
        return operational("clock unavailable — pass --now-ms");
    };
    let root = match open_root(&a.data_dir, a.replica.as_deref()) {
        Ok(r) => r,
        Err(_) => return operational("store unreachable — run: doctor"),
    };
    // brief 不建索引也能跑：`tools::status` 的索引那一格报 built:false（任务 8）。
    // 这正是 brief 与 probe 的分工——状态块是读数，读数不许依赖检索面。
    let json = match rc_agent::tools::status(root.memory(), None, &rc_agent::tools::StatusOptions::default(), now) {
        Ok(j) => j,
        Err(e) => return emit_error(&e, 3),
    };
    // W34 续接包：固定段「上一棒交接」。只有给了 `--agent-id` 才拼；段缺席时
    // `brief_block` 的输出与 `status_block` 逐字节相同，既有形状一字不动。
    let section = match &a.agent_id {
        None => rc_core::render::HandoffPrevSection::default(),
        Some(agent_id) => match handoff_prev(&a.data_dir, agent_id, root.memory(), now) {
            Ok(s) => s,
            Err(rc_core::CoreError::BadAgentId) => {
                // 2 = 输入不可用（与用法错误同码）：agent id 过不了 safe_token，
                // 重试一万次还是这个错，落 3 会把操作员引去跑 doctor。
                let e = rc_core::CoreError::BadAgentId;
                use std::io::Write;
                let _ = std::io::stderr().write_all(format!("error: {e}\n").as_bytes());
                return ExitCode::from(2);
            }
            // 库在但读不了（key 缺、密文解不开、sqlite 报错）：运维失败，绝不折成沉默（G7）。
            Err(_) => return operational("core store unreadable — run: doctor"),
        },
    };
    // W34 评审缺陷①的修法：截断发生过必须走 note 通道说出声——`render_handoff_prev`
    // 超过 [`rc_core::render::CAP_HANDOFF_PREV`] 静默截断时，读的人会以为交接段是完整的。
    // 段文本照发（截断是读数面的已知边界，不是错误，退出码与 stdout 一字不动）。
    if section.truncated {
        note("prev-handoff truncated at CAP_HANDOFF_PREV");
    }
    if emit(&rc_agent::brief_block(&json.text(), &section.text)).is_err() {
        return operational("stdout closed by host");
    }
    ExitCode::from(0)
}

/// 「上一棒交接」段的读侧组装（W34 续接包）。**纯读侧**：只调既有的
/// `CoreStore::handoff_list` 读取面，handoff 的 open/close 语义与生命周期一字不动。
///
/// - 未闭条目：`handoff_list` 的返回形状——未关闭、**含已过期**（过期与否由段内
///   `render_due` 的"已过期 N 天"标出）、已按 kind 序排好，这里不再排第二遍。
/// - 读数：只挂**一格**——审核队列待审数 `pending_review`（`rc_memory::review::stats`）。
///   **同一键名、两本账，别混**（W34 评审缺陷③的口径修正）：这一格与 doctor 的 review
///   对账读的是**实表**（同一个 `stats` 函数）；而 `hub pending` 的 `review.pending` 读的是
///   **审查轴水位的镜像**（`review_mark`，`hub/src/pending.rs`）。两者只有在水位被人
///   （`hub review decide`、引擎收尾、mcp report 臂的补写）刷新到与实表同步时才相等；
///   新入队项在水位重算之前，实表侧比水位多——那是两本账的正常差，不是对账错误。
///   lag 格在 hook 面拿不到真值：推导游标住水位账（rc-ledger，守卫 6 禁 hook 依赖），
///   没有游标的行数只是"全库活行"，不是欠账——宁缺一格，不造假读数。
///   读数为 0 不出声（报数是因为有账）；读失败走 stderr note，不折成 0。
/// - 沉默匹配视图内容：core.db 不存在 / agent 未注册 / 没写过文档 ⇒ 空段（缺席，不是
///   空标题）；有未闭条目或非零读数 ⇒ 段出现。
/// - 错误分型照 brief 的退出码契约：`BadAgentId` ⇒ 2（输入不可用）；其余 ⇒ 3
///   （运维失败绝不折成沉默，G7）。`Unregistered`/`NotFound` 不是错误，是合法的空。
fn handoff_prev(
    data_dir: &Path,
    agent_id: &str,
    m: &rc_store::MemoryHandle,
    now: i64,
) -> Result<rc_core::render::HandoffPrevSection, rc_core::CoreError> {
    // 先判存在再 open：`CoreStore::open` 会建库（全局表 + WAL），读路径不许带这个副作用。
    if !data_dir.join("core.db").exists() {
        return Ok(rc_core::render::HandoffPrevSection::default());
    }
    let store = rc_core::CoreStore::open(data_dir)?;
    let items = match store.handoff_list(agent_id, now) {
        Ok(items) => items,
        Err(rc_core::CoreError::Unregistered | rc_core::CoreError::NotFound) => {
            return Ok(rc_core::render::HandoffPrevSection::default());
        }
        Err(e) => return Err(e),
    };
    let mut readings = Vec::new();
    match rc_memory::review::stats(m, now) {
        Ok(s) if s.pending_review > 0 => {
            readings.push(format!("- 项目状态：审核队列待审 {} 条", s.pending_review));
        }
        Ok(_) => {}
        Err(_) => note("review stats unreadable"),
    }
    Ok(rc_core::render::render_handoff_prev(&items, &readings, now))
}

fn run_probe(a: &ProbeArgs) -> ExitCode {
    let Some(now) = clock(a.now_ms) else {
        return operational("clock unavailable — pass --now-ms");
    };
    // 读者身份（W37-A1）：给了 `--agent-id` 才有。这里**不做** `safe_token` 形状校验——
    // 这个 id 只进读闸的相等比较（owner_agent_id / source_agent 比对），不落库、不进任何
    // 主语位；形状奇怪的 id 只会让它匹配不到任何行（fail-closed 方向），不是注入面。
    let probe_viewer = a.agent_id.as_ref().map(|id| rc_agent::recall::Viewer::agent(id.clone()));
    let root = match open_root(&a.data_dir, a.replica.as_deref()) {
        Ok(r) => r,
        Err(_) => return operational("store unreachable — run: doctor"),
    };
    // 给了 `--query` 就**绝不读 stdin**：一次多余的阻塞读能把整场会话卡住。
    // `read_stdin()` 给 `None`（读不出来）时 blob 保持**空**：判定与退出码契约不变（仍由
    // `validate_query` 落 2），而"读不出来"这半边的症状已经在那条 note 里说出去了（R77）。
    // 这里偏不写成 `unwrap_or_default()`——"把读失败折成一个默认值"正是本轮在治的病。
    let mut blob = String::new();
    // `host_stdin`：输入确实从宿主的 stdin 读成功了。只有这一支允许把"没有词"读成"宿主没话"。
    let mut host_stdin = false;
    if let Some(q) = &a.query {
        blob.push_str(q);
    } else if let Some(s) = read_stdin() {
        blob.push_str(&s);
        host_stdin = true;
    }
    let q = match rc_agent::recall::validate_query(&host_prompt(&blob)) {
        Ok(q) => q,
        // 宿主灌进空 prompt 是常态（附件-only / slash / resume），不是操作员的错。
        // 而 exit 2 在 UserPromptSubmit 上 = 阻断该 prompt ⇒ 记忆系统绝不能挡住用户说话。
        // 沉默 = 零字节 + 0，与"命中但全被闸门挡掉"字节相同，这是设计要的形状。
        // **`read_stdin()` 失败那一向不在此列**：它没被标成 host_stdin，仍落 2 + `stdin unread`。
        Err(e) if host_stdin && e.code == "empty_query" => return ExitCode::from(0),
        // 2：输入不可用。与 3 分开的那条理由原样继承 hub——坏提示重试一万次还是坏提示，
        // 而库开不通重试可能就是好的。
        Err(e) => return emit_error(&e, 2),
    };
    let view = match probe_via_search(root.memory(), probe_viewer.as_ref(), &q, now) {
        Ok(v) => v,
        // 索引未建 / 尺子版本不符 / schema 太新：全是运维失败，且**绝不表现为沉默**。
        // "没建索引"与"库里没有"字节相同是 G7 头号的失效形状。
        Err(e) => return emit_error(&e, 3),
    };
    let block = probe_block(&view);
    if emit(&block).is_err() {
        return operational("stdout closed by host");
    }
    // 全系统唯一"读路径上写库"的地方（V7）。块已经决定完了：写失败绝不回头撤掉它，
    // 否则观测反过来控制行为；但也不许静默，note 一行类别名进 stderr。
    if !block.is_empty() {
        stamp_cooldown(root.memory(), &view, now);
    }
    // V4 分片：只在**真的出声**且调用方给了会话身份时写。没有 `--session` 时这一段零 I/O——
    // 宿主不开分片就一个目录都不建。写失败只 note 一条类别串（零路径），**不改退出码**：
    // 注入块已经决定并发出去了，观测不许反过来撤掉行为；但也不许静默。
    if let Some(session) = &a.session {
        if !block.is_empty() {
            let keys: Vec<String> = view.forms.iter().map(|d| d.key.key_string()).collect();
            if rc_store::shards::record(&a.data_dir, session, &keys, now).is_err() {
                note("shard write failed");
            }
        }
    }
    ExitCode::from(0)
}

fn stamp_cooldown(m: &rc_store::MemoryHandle, view: &rc_search::gate::ProbeView, now: i64) {
    // 裁定 R56（本段对 brief 那块代码唯一的码级增补，逐字理由如下）：canonical `anchor` 那张表没有
    // `cooldown_until_ms` 列，于是 `bump_cooldown` 撞见 Anchor 句柄时是**整批 `return Err
    // (MemError::NotFound)`**（`crates/rc-memory/src/writers_p.rs` 的 `if t.table ==
    // MemTable::Anchor` 那一支：`return`，不是 `continue`），而不是"跳过那一条"。
    // **S2 之后这条链从 CLI 走不到**：brief/probe 的候选集已经剔掉锚（`rc-agent` 那一刀），
    // 于是下面这个过滤今天是**防御性**的冗余，不是可达路径。保留它是因为链本身是真的、
    // 逐环对过位点：`rc_memory::reader::facts` 的 `ROW_TABLES` 含 Anchor ⇒ `rc_index::rebuild`
    // 把每一行都灌进影子索引（实体的标题即全部可检索内容）⇒ `rc_search::gate::decide` 把
    // `s.key` 原样带进 `Decision` ⇒ 任何又把锚放进候选的调用面（或将来某次改回去）都会让一条
    // 命中的实体形态出现在 `view.forms` 里。后果是**静默降级**而非崩溃：块照样出声、退出码照样 0，
    // 可同批 card/event/assertion 的冷却**一格都没盖**（Anchor 之前的已逐条 autocommit 写进去了，
    // 之后的全没写到）——闸门看起来在工作，实际每轮重复注入同一批。
    // 为什么保持**一次** batch 调用、不逐句柄各调一次：`Ok(n) => note("cooldown stamped
    // {n}/{len}")` 那条回执判据（R30）读的是"一次调用的 touched 与 keys.len()"，逐柄会把
    // 它摊成 N 条 note，等于把回执这个读数拆掉。
    let keys: Vec<rc_memory::keys::MemKey> = view
        .forms
        .iter()
        .filter(|d| d.key.table != rc_memory::keys::MemTable::Anchor)
        .map(|d| d.key.clone())
        .collect();
    // 这里**不**先问 `m.is_read_only()`：只读判定住在 `writers_p::bump_cooldown` 里（任务 3），
    // 两处各判一次就是两份"什么叫只读"。返回的错误是唯一读数。
    // 批里全是实体形态时 `keys` 为空：只读柄仍在上面那支返回 `ReadOnly`（副本面的 note 不丢），
    // 可写柄给 `Ok(0)` 且 `0 == keys.len()` ⇒ 走 `Ok` 的第一支，正当 no-op，不打 note。
    match bump_cooldown(m, &keys, now, COOLDOWN_MS) {
        Ok(n) if n == keys.len() => {}
        Ok(n) => note(&format!("cooldown stamped {n}/{}", keys.len())),
        Err(rc_memory::MemError::ReadOnly) => note("replica_read_only cooldown not stamped"),
        Err(_) => note("cooldown stamp failed — run: doctor"),
    }
}

fn read_stdin() -> Option<String> {
    let mut s = String::new();
    // **读不出来 ≠ 没有输入**（R77）：前者是宿主/管道的事，next_action 是"查宿主配置"；
    // 后者是提示词的事，next_action 是"改提示词"。把两者折成一条串就是 `rc-agent/src/error.rs`
    // 那条纪律在 CLI 侧的失效形状（库读不到被报成记忆为空）。这里只出声，不猜类别：
    // `Err(other)`（BrokenPipe / UnexpectedEof）从 CLI 造不出夹具，给它单独一支等于新造一条
    // 零读者分支，所以是单臂 + 一条有读者的 note。
    match std::io::stdin().read_to_string(&mut s) {
        Ok(_) => Some(s),
        Err(_) => {
            note("stdin unread — pass --query");
            None
        }
    }
}

/// 宿主在 `UserPromptSubmit` 上灌进来的常常是一个 JSON。**只认一件事**：它是对象、
/// 且 `prompt` 是字符串 ⇒ 取那一字段；否则整块原样当查询。
/// 不解析 `session_id`：D2 的会话身份只从显式的 `--session` 进（分片键），不从宿主 JSON 里猜；
/// 认了第一个字段就会想认第二个。**也不碰 `transcript_path`**：那个文件本 target 看不见（守卫 6）。
fn host_prompt(blob: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(blob) {
        if let Some(p) = v.get("prompt").and_then(|p| p.as_str()) {
            // 空 prompt **不回退**整块：回退等于把 session_id 和路径喂进分词器，
            // 那是守卫 10 在写入侧防的那类洪水搬到查询侧。
            return cap(p);
        }
    }
    cap(blob)
}

/// 单次查询的字符上限。为什么截断而不是拒收：长提示是**合法输入**，拒掉它等于
/// "用户话越长、记忆越沉默"——正是 G7 要防的不可解释沉默。截断只影响尾部的词能不能命中，
/// 而这个损失必须说出去（C3 那条"不许静默截断"的尺子）。**待实测常数**：阶段 5 与
/// `hook` 全链 150 ms 预算一起收口（进任务 11 的台账）；本阶段的判据只有"超限仍出声且必报"。
const MAX_PROBE_QUERY_CHARS: usize = 2000;

fn cap(s: &str) -> String {
    let n = s.chars().count();
    if n <= MAX_PROBE_QUERY_CHARS {
        return s.to_string();
    }
    note(&format!("query truncated {n}->{MAX_PROBE_QUERY_CHARS} chars"));
    s.chars().take(MAX_PROBE_QUERY_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json里取prompt_非json取整块_空prompt不回退() {
        assert_eq!(host_prompt(r#"{"prompt":"甲乙"}"#), "甲乙");
        assert_eq!(host_prompt("裸文本 甲乙"), "裸文本 甲乙");
        // 空 prompt 原样交给上层校验（`cap` 不动它，`validate_query` 拒它）。
        assert_eq!(host_prompt(r#"{"prompt":"  "}"#), "  ");
        // prompt 存在时，同层的 session_id 与路径一个字都不进查询。
        let blob = r#"{"session_id":"/tmp/x","transcript_path":"/db/t.db","prompt":"甲乙"}"#;
        let got = host_prompt(blob);
        assert_eq!(got, "甲乙");
        assert!(!got.contains("session_id") && !got.contains("/db/"), "取到了别的字段：{got}");
    }

    #[test]
    fn 截断按字符不按字节() {
        // 3000 个"记"= 9000 字节。按字节截会在 UTF-8 中间断掉（panic 或乱码）。
        let s = "记".repeat(3000);
        let c = cap(&s);
        assert_eq!(c.chars().count(), MAX_PROBE_QUERY_CHARS);
        assert_eq!(c, "记".repeat(MAX_PROBE_QUERY_CHARS));
    }

    #[test]
    fn 空白与非空白在同一个函数上分界() {
        assert!(rc_agent::recall::validate_query("   ").is_err());
        assert!(rc_agent::recall::validate_query("").is_err());
        assert_eq!(rc_agent::recall::validate_query(" 甲 ").unwrap(), " 甲 ");
    }
}
