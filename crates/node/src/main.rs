//! `recollect-node` 的 CLI。**三个子命令**：`pair` / `sync` / `status`。
//!
//! `--discover` 是第四条独立的路：扫描 LAN、印出候选、立即退出。它**不读配置**，
//! 因为它回答的正是配对之前的问题（"中心在哪"）。
//!
//! `pair` 有两个凭证入口，**互斥**：`--code`（旧路，中心出的配对码）与
//! `--invite`（票：自带地址与 TLS 指纹，加入后端点落进状态文件）。
//!
//! 这层只做：解析旗标、读配置、调 `node::sync`、把结果印成一行 JSON。
//! 判定全在库里。
//!
//! **不弹前台控制台、不提示输入**：本进程由外部调度器调用，
//! 没有 TTY 也不该要任何输入。`--confirm` 那类旗标是**唯一**的确认方式。

use clap::Parser;
use node::node_cfg;
use node::selfdistill;
use node::state;
use node::sync::{self, Mode, SyncPlan};
use rc_net::key::DeviceKey;
use std::io::Write;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "recollect-node", version)]
struct Cli {
    #[arg(long, default_value = "./node.toml")]
    config: PathBuf,
    #[arg(long, default_value = "10")]
    timeout_s: u64,
    /// `pair` 用的配对码。**只从旗标拿**，不从 stdin 读（非 TTY 不提示输入）。
    #[arg(long, conflicts_with = "invite")]
    code: Option<String>,
    /// `pair` 用的邀请票（`hub invite` 印出的那一条 `rc1.…` 文本）。
    /// 与 `--code` **互斥**：两个凭证同给是用法错误，不是"先试一个再试另一个"。
    #[arg(long, conflicts_with = "code")]
    invite: Option<String>,
    #[arg(long)]
    once: bool,
    #[arg(long)]
    push: bool,
    #[arg(long)]
    pull: bool,
    /// 朝 LAN 发一条 probe，把应答的中心列出来就退出。**不需要 `node.toml`**：
    /// 它要回答的正是"配对之前那台中心在哪"，那时这台机器还没有配置。
    #[arg(long)]
    discover: bool,
    /// 只保留这一枚 hub 指纹（64 hex）。给了它而一个都没扫到 ⇒ 退出 4，
    /// 不是"空表 + 0"——那是"你要的那台中心不在"，和"这片网上没有中心"是两件事。
    #[arg(long, value_name = "64HEX")]
    expect_fp: Option<String>,
    /// 发现的目标端口。默认与中心应答用的固定端口同一个数（两处各写一遍就是下一次
    /// "节点问 37420、中心答 37421"的来源）。
    #[arg(long, default_value_t = rc_net::discovery::DISCOVERY_PORT)]
    discovery_port: u16,
    /// 发现的目标地址。默认全子网广播；测试用 127.0.0.1 把"扫描"钉在本机。
    #[arg(long, default_value = "255.255.255.255")]
    discovery_target: String,
    /// 机器可读输出。默认输出的**就是** JSON 一行，这个旗标只为与 `hub` 的用法对齐。
    #[arg(long)]
    json: bool,
}

fn main() -> ExitCode {
    let cli = Cli::try_parse();
    let cli = match cli {
        Ok(c) => c,
        Err(e) if e.use_stderr() => {
            return operational("invalid invocation — usage: recollect-node <pair|sync|status> --config FILE | recollect-node --discover [flags]");
        }
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(0);
        }
    };
    // `--discover` 必须在读配置**之前**分派：发现是配对之前的事，那时候这台机器
    // 还没有（也不该为了找中心先造一个假的）`node.toml`。
    if cli.discover {
        return run_discover(&cli);
    }
    let cfg = match node_cfg::load(&cli.config) {
        Ok(Some(c)) => c,
        Ok(None) => return operational("no node configuration — run: doctor"),
        Err(e) => return operational(&e),
    };
    let mut st = match state::load(&state::state_path(&cfg.data_dir)) {
        Ok(Some(s)) => s,
        Ok(None) => state::SyncState::default(),
        Err(e) => return operational(&e),
    };
    let timeout = Duration::from_secs(cli.timeout_s);
    let now_ms = now_ms();
    // 进程内单调钟；跨进程靠状态文件里的 max。墙钟回拨时它是最后一道保险。
    let monotonic = std::time::Instant::now();

    if let Some(code) = cli.code.as_deref() {
        return run_pair(&cfg, code, &mut st, timeout);
    }
    if let Some(ticket) = cli.invite.as_deref() {
        return run_pair_invite(&cfg, ticket, &mut st, timeout, now_ms);
    }
    if cli.once || cli.push || cli.pull {
        let mode = if cli.push {
            Mode::Push
        } else if cli.pull {
            Mode::Pull
        } else {
            Mode::Both
        };
        return run_sync(
            &cfg,
            &mut st,
            mode,
            now_ms,
            monotonic.elapsed().as_millis() as i64,
            timeout,
        );
    }
    // 没有旗标 = `status`：把状态文件念一遍。**不联网**。
    print_status(&st, &cfg);
    ExitCode::from(0)
}

/// 扫描旗标的静态用法句。**不回显用户输入**：这一行与 `operational` 同一类（守卫 12），
/// 坏掉的指纹/目标地址不需要被抄进日志。发现层只绑 `0.0.0.0`，所以目标是 **IPv4 only**。
const DISCOVER_USAGE: &str = "invalid invocation — usage: recollect-node --discover [--expect-fp 64HEX] [--discovery-port N] [--discovery-target IP] [--timeout-s N] [--json] (IPv4 only)";

/// 发现：朝目标发一条 probe，把验得过的公告印成一行 JSON 就退出。**不读盘、不写盘**。
///
/// 三个退出码各说一件事：
/// - 0：扫描完成。找到候选当然算；**没找到但也没有 `--expect-fp` 也算**——一片还没有
///   中心的网，发现命令的正常读数就是空表（把它做成失败会让第一次开机必须一次成功）；
/// - 4：`--expect-fp` 指定了，但扫完一个都不匹配。用户要的那台中心不在，与"网上没有中心"
///   是两件事，退出码必须能分开（脚本据此决定"再扫"还是"去看中心起没起"）；
/// - 3：用法错（指纹不是 64 hex、目标不是 IP）或扫描本身失败（绑不上 socket 等 IO）。
fn run_discover(cli: &Cli) -> ExitCode {
    // `--json` 与默认输出是同一份文档，这个旗标只为与 `hub invite --json` 同一套用法。
    let _ = cli.json;
    if let Some(raw) = cli.expect_fp.as_deref() {
        if raw.len() != 64 || !raw.bytes().all(|b| b.is_ascii_hexdigit()) {
            return operational(DISCOVER_USAGE);
        }
    }
    // 指纹统一按小写比：`hub_fingerprint` 产出的就是小写 hex，大写的输入是同一枚指纹。
    let expected = cli.expect_fp.as_deref().map(|s| s.to_ascii_lowercase());
    let Ok(ip) = cli.discovery_target.parse::<IpAddr>() else {
        // 目标只收 IP：主机名要 DNS，而发现是"连 DNS 都没有时也能用"的那条路。
        return operational(DISCOVER_USAGE);
    };
    // 发现层只绑 `0.0.0.0`：IPv6 目标会在 send_to 阶段失败并被折成"中心不可达"，
    // 那会把一次用法错误读成网络问题。这里明确拒绝（静态句，不回显地址）。
    if ip.is_ipv6() {
        return operational("invalid invocation — discovery is IPv4 only");
    }
    let targets = [SocketAddr::new(ip, cli.discovery_port)];
    // 钟在**入口**取（判据 12a：库里不取钟）；发现层只拿这个数去比公告的窗口。
    let found = match rc_net::discovery::discover(
        &targets,
        Duration::from_secs(cli.timeout_s),
        now_ms(),
    ) {
        Ok(v) => v,
        Err(e) => return operational(&format!("discovery scan failed — {e}")),
    };
    // 空表时在 stderr 印一行读数：**不印地址**，只说"问了几处、等了多久、没人答"。
    // 有候选时不印（stdout 仍是那唯一一行 JSON）。
    if found.is_empty() {
        let _ = std::io::stderr().write_all(
            format!(
                "discovery: {} target, {}ms, no candidate\n",
                targets.len(),
                cli.timeout_s.saturating_mul(1000)
            )
            .as_bytes(),
        );
    }
    let kept: Vec<&rc_net::discovery::Candidate> = match &expected {
        Some(fp) => found.iter().filter(|c| &c.hub_fp == fp).collect(),
        None => found.iter().collect(),
    };
    // `seen_from` 不印：它是本机的网络细节，不是候选的身份。票与 token 也不在这条路上。
    let doc = serde_json::json!({
        "candidates": kept
            .iter()
            .map(|c| serde_json::json!({
                "hub_fp": c.hub_fp,
                "tls_fp": c.tls_fp,
                "epoch": c.epoch,
                "endpoints": c.endpoints,
                "hub_pub": c.hub_pub,
            }))
            .collect::<Vec<_>>(),
    });
    println!("{doc}");
    if expected.is_some() && kept.is_empty() {
        return ExitCode::from(4);
    }
    ExitCode::from(0)
}

fn run_pair(
    cfg: &node_cfg::NodeConfig,
    code: &str,
    st: &mut state::SyncState,
    timeout: Duration,
) -> ExitCode {
    let key = match load_or_create_key(&cfg.device_key_path) {
        Ok(k) => k,
        Err(e) => return operational(e),
    };
    let ep = match cfg.endpoints.first() {
        Some(e) => e,
        // 旧路只走配置里手写的端点：没有端点就没有"往哪儿发码"这回事。
        None => return operational(sync::SyncError::NoEndpoint.code()),
    };
    let paired = match sync::pair(
        &rc_net::node::Endpoint {
            label: ep.label.clone(),
            host: ep.host.clone(),
            port: ep.port,
            // 与 `sync` 同一套有效钉死值：端点没写就用配置/状态里的那一枚。
            tls_fp: ep.tls_fp.clone().or_else(|| effective_tls_fp(cfg, st)),
        },
        code,
        &key,
        timeout,
    ) {
        Ok(p) => p,
        Err(e) => return operational(e.code()),
    };
    if let Err(e) = sync::write_token(&cfg.data_dir, &paired.token) {
        return operational(e.code());
    }
    st.paired_hub_pub = paired.hub_pub.clone();
    if let Err(e) = state::save(&state::state_path(&cfg.data_dir), st) {
        return operational(&e);
    }
    println!(
        "{{\"paired\":true,\"device_id\":\"{}\",\"hub_pub\":\"{}\",\"announced_endpoints\":{}}}",
        key.device_id(),
        paired.hub_pub,
        paired.endpoints.len()
    );
    ExitCode::from(0)
}

/// 凭邀请票加入：**票是地址与信任的唯一来源**，所以这里一个端点都不从 `node.toml` 读。
///
/// 1. 拆信封：拆不开就是 `invite_invalid`（不回显票）；
/// 2. 本地判过期：票里写着 `expires_at_ms`，先判一次省掉一发必然被拒的请求；
/// 3. 把票里的端点变成"路"：只认 `https://IP:port`，空列表/坏地址都是 `invite_invalid`；
/// 4. 逐条试（顺序同票）：`/v1/identity` 验签+三比较 → `/v1/pair` 换 token；
/// 5. 成功后把 token、中心公钥、**TLS 指纹与端点**一起落盘——下一次 `sync`
///    不再需要 `node.toml` 里有端点。
fn run_pair_invite(
    cfg: &node_cfg::NodeConfig,
    ticket_text: &str,
    st: &mut state::SyncState,
    timeout: Duration,
    now_ms: i64,
) -> ExitCode {
    let ticket = match rc_net::invite::decode_ticket(ticket_text) {
        Ok((t, _sig)) => t,
        Err(_) => return operational(sync::SyncError::InviteInvalid.code()),
    };
    // 本地过期判定与中心那一侧同一格：`expires_at_ms < now` 就是不能用（边界算有效）。
    if ticket.expires_at_ms < now_ms {
        return operational(sync::SyncError::InviteExpired.code());
    }
    let endpoints = match sync::ticket_endpoints(&ticket) {
        Ok(e) => e,
        Err(e) => return operational(e.code()),
    };
    let key = match load_or_create_key(&cfg.device_key_path) {
        Ok(k) => k,
        Err(e) => return operational(e),
    };
    let paired = match sync::pair_invite(&endpoints, ticket_text, &ticket, &key, timeout) {
        Ok(p) => p,
        Err(e) => return operational(e.code()),
    };
    if let Err(e) = sync::write_token(&cfg.data_dir, &paired.token) {
        return operational(e.code());
    }
    st.paired_hub_pub = paired.hub_pub.clone();
    // 指纹与端点来自**票**（被签名的那一份），不是应答里的：应答可以少字段，
    // 票不能——票里的地址就是"这张纸让人去连哪儿"。
    st.paired_tls_fp = Some(ticket.tls_fp.clone());
    st.paired_endpoints = ticket.endpoints.clone();
    if let Err(e) = state::save(&state::state_path(&cfg.data_dir), st) {
        return operational(&e);
    }
    println!(
        "{{\"paired\":true,\"device_id\":\"{}\",\"hub_pub\":\"{}\",\"announced_endpoints\":{}}}",
        key.device_id(),
        paired.hub_pub,
        paired.endpoints.len()
    );
    ExitCode::from(0)
}

fn run_sync(
    cfg: &node_cfg::NodeConfig,
    st: &mut state::SyncState,
    mode: Mode,
    now_ms: i64,
    monotonic_elapsed_ms: i64,
    timeout: Duration,
) -> ExitCode {
    // 有效端点 = 配置端点（原顺序）**再接**状态里 `host:port` 没出现过的那几条。
    // 只取配置会让"配置里写了旧地址、状态里有发现来的新地址"永远撞旧地址；
    // 只取状态会让手写的清单失效。合并之后两条都试，发现结果也不会被旧地址挡住。
    let endpoints = effective_endpoints(cfg, st);
    if endpoints.is_empty() {
        return fail_early(cfg, st, sync::SyncError::NoEndpoint.code(), now_ms);
    }
    let key = match rc_net::key::DeviceKey::load(&cfg.device_key_path) {
        Ok(k) => k,
        Err(_) => return fail_early(cfg, st, "bad_token_file", now_ms),
    };
    let token = match std::fs::read_to_string(state::token_path(&cfg.data_dir)) {
        Ok(t) => t.trim().to_string(),
        Err(_) => return fail_early(cfg, st, "no_pairing", now_ms),
    };
    // 钉死值优先：**配置钉死的公钥与配对时拿到的不是同一把 ⇒ 响亮失败**（没有 TOFU）。
    if let Some(pin) = &cfg.hub_pub {
        if !st.paired_hub_pub.is_empty() && *pin != st.paired_hub_pub {
            return fail_early(cfg, st, "hub_key_mismatch", now_ms);
        }
    }
    let hub_pub = effective_hub_pub(cfg, st);
    // S4d1：重发现的目标与窗口。目标默认广播（`255.255.255.255`），端口默认与
    // `--discover` 同一个数；窗口缺省 600ms（`discovery_timeout_ms`）——发现是
    // "顺手问一句"，一次同步不该为一个不肯应答的网段多等一整个 `timeout_s`。
    let discovery_target = cfg
        .discovery_target
        .as_deref()
        .and_then(|t| t.parse::<IpAddr>().ok())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::BROADCAST));
    let discovery_port = cfg.discovery_port.unwrap_or(rc_net::discovery::DISCOVERY_PORT);
    // `discovery = false` ⇒ 空表 = 本计划不发现（关掉自动重连）。
    let discovery = if cfg.discovery {
        vec![SocketAddr::new(discovery_target, discovery_port)]
    } else {
        Vec::new()
    };
    let plan = SyncPlan {
        endpoints,
        token,
        device_id: key.device_id(),
        key: &key,
        hub_pub,
        // 连接与发现共用同一枚有效指纹（配置优先、其次状态）。
        hub_tls_fp: effective_tls_fp(cfg, st),
        data_dir: cfg.data_dir.clone(),
        discovery,
        discovery_timeout: Duration::from_millis(cfg.discovery_timeout_ms),
    };
    match sync::sync_once(&plan, st, mode, now_ms, timeout) {
        Ok(outcome) => {
            node::offline::observe_success(st, now_ms);
            // 这一轮成功了：上次失败的码不再描述现状。`last_error_at_ms` **不清**——
            // 它回答的是"那个错是什么时候的事"，清掉就分不出陈旧错误与当前错误。
            st.last_error.clear();
            // 中心确认收下这一轮回并 ⇒ 记下保留窗口的锚。0 是"未确认"，
            // 从这一刻起 `dr::rotate` 才允许删早于确认时刻的旧自备份。
            if outcome.reconciled {
                st.reconcile_confirmed_at_ms = now_ms;
            }
            // 中心回来了，但这一轮回并没走完（无快照 / 层没丢成）：窗口清零，状态仍是
            // `reconciling`。写成 `normal` 会让"暂存层还在"与读数互相矛盾。
            if node::staging::is_active(&cfg.data_dir) && !outcome.reconciled {
                st.self_distill = state::SelfDistill::Reconciling;
            }
            // 成功语义同样记一次钥匙面：这一格是"这台机器现在能不能开箱"，由配置与箱现算。
            // 暂存层不删、不回并——那是 B3 的事。
            st.llm_key = Some(selfdistill::live_key_reading(cfg, &cfg.data_dir).to_string());
            if let Err(e) =
                node::index::ensure_current_replica(&cfg.data_dir, &st.current_manifest, now_ms)
            {
                st.last_error = e.code().to_string();
                st.last_error_at_ms = now_ms;
                let _ = state::save(&state::state_path(&cfg.data_dir), st);
                return operational(e.code());
            }
            // 清理只能在状态落盘之后：状态还指着旧代时删旧代，中途崩掉就会留下
            // "状态点名一份不存在的副本"。
            if state::save(&state::state_path(&cfg.data_dir), st).is_ok() {
                state::cleanup_old_replicas(&cfg.data_dir, &st.current_manifest);
            }
            println!(
                "{{\"pushed\":{},\"push_skipped\":{},\"pulled\":{},\"pull_skipped\":{},\"no_memory\":{},\"endpoint\":\"{}\",\"resumes\":{},\"self_distill\":\"{}\",\"offline_elapsed_ms\":{}}}",
                outcome.pushed,
                outcome.push_skipped,
                outcome.pulled,
                outcome.pull_skipped,
                outcome.no_memory,
                outcome.endpoint,
                outcome.resumes,
                st.self_distill.as_str(),
                st.offline_elapsed_ms
            );
            ExitCode::from(0)
        }
        Err(e) => {
            st.last_error = e.code().to_string();
            st.last_error_at_ms = now_ms;
            // 1 天闸真接：满一天且 `auto_unlock` ⇒ `OfflineDistilling`（判定在 `offline` 里，这里只递配置）。
            node::offline::observe_failure(st, e, now_ms, monotonic_elapsed_ms, cfg.auto_unlock);
            st.llm_key = Some(selfdistill::live_key_reading(cfg, &cfg.data_dir).to_string());
            if st.self_distill == state::SelfDistill::OfflineDistilling {
                let meter = Rc::new(rc_cycle::Metered::new());
                if let Ok(backend) = selfdistill::build_live_backend(cfg, &cfg.data_dir, meter.clone()) {
                    // 批次失败不改同步的退出码：这一轮的判定仍是 `endpoint_unreachable`。
                    let _ = selfdistill::run_one_offline_batch(&cfg.data_dir, st, &backend, &meter, now_ms);
                }
            }
            // 第 5 天自升：detached 两态里每 ≥1 小时自备份一次。失败静默跳过
            // （`NoReplica`/`NoState` 就是还没备成，doctor 的读数为 0），不改同步退出码。
            if st.self_distill.is_detached() {
                let due =
                    st.dr_backup_last_at_ms == 0 || now_ms - st.dr_backup_last_at_ms >= 3_600_000;
                if due {
                    if let Ok(rep) = node::dr::backup_once(&cfg.data_dir, st, now_ms) {
                        let _removed =
                            node::dr::rotate(&cfg.data_dir, st.reconcile_confirmed_at_ms, 5)
                                .unwrap_or(0);
                        st.dr_backup_last_at_ms = now_ms;
                        st.dr_backup_count += 1;
                        st.local_turn_highwater_ms = rep.local_turn_highwater_ms;
                        st.self_distill = state::SelfDistill::DetachedPromoted;
                    }
                }
            }
            let _ = state::save(&state::state_path(&cfg.data_dir), st);
            operational(e.code())
        }
    }
}

/// 有效中心公钥：配置钉死的优先，否则用配对时拿到的那一把。连接与发现共用。
fn effective_hub_pub(cfg: &node_cfg::NodeConfig, st: &state::SyncState) -> String {
    match &cfg.hub_pub {
        Some(pin) => pin.clone(),
        None => st.paired_hub_pub.clone(),
    }
}

/// 有效证书指纹：配置 `hub_tls_fp`（非空）优先，否则状态里票带来的那一枚。
/// **连接与发现共用这一格**：两套钉死值会让证书轮换后误报 `identity_mismatch`，
/// 也会让纯配置配好的节点永远不开发现。
fn effective_tls_fp(cfg: &node_cfg::NodeConfig, st: &state::SyncState) -> Option<String> {
    cfg.hub_tls_fp
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| st.paired_tls_fp.clone().filter(|s| !s.is_empty()))
}

/// 有效端点清单：配置端点（原顺序，各自 `tls_fp` 优先、否则用有效指纹）**再接**
/// 状态里 `host:port` 没出现过的那几条（用有效指纹）。
///
/// `paired_endpoints` 仍然只存票/发现来的地址——配置里的地址是运维的事，不往状态里抄。
fn effective_endpoints(
    cfg: &node_cfg::NodeConfig,
    st: &state::SyncState,
) -> Vec<rc_net::node::Endpoint> {
    let tls_fp = effective_tls_fp(cfg, st);
    let mut out: Vec<rc_net::node::Endpoint> = cfg
        .endpoints
        .iter()
        .map(|e| rc_net::node::Endpoint {
            label: e.label.clone(),
            host: e.host.clone(),
            port: e.port,
            tls_fp: e.tls_fp.clone().or_else(|| tls_fp.clone()),
        })
        .collect();
    // 路名序号接着配置的条数往下排：`paired-N` 与配置 label 撞名会让
    // `order_endpoints` 的"上次那条先试"指到另一条物理端点上。
    for (i, url) in st.paired_endpoints.iter().enumerate() {
        let Some(ep) =
            sync::endpoint_from_https_url(url, tls_fp.as_deref(), cfg.endpoints.len() + i)
        else {
            continue;
        };
        if out.iter().any(|e| e.host == ep.host && e.port == ep.port) {
            continue;
        }
        out.push(ep);
    }
    out
}

/// 早退分支的统一出口：**记码 + 记时刻 + 落盘**，再按运维失败退出。
///
/// `no_endpoint` / `bad_token_file` / `no_pairing` / `hub_key_mismatch` 都在"还没开口"
/// 之前，所以不走 `observe_failure`：中心答没答这件事它们证明不了，动离线窗口只会误导读数。
fn fail_early(
    cfg: &node_cfg::NodeConfig,
    st: &mut state::SyncState,
    code: &str,
    now_ms: i64,
) -> ExitCode {
    st.last_error = code.to_string();
    st.last_error_at_ms = now_ms;
    let _ = state::save(&state::state_path(&cfg.data_dir), st);
    operational(code)
}

fn print_status(st: &state::SyncState, cfg: &node_cfg::NodeConfig) {
    // 三格现算，不从状态文件抄：`paired` 看的是"有没有中心公钥 + token 文件在不在"，
    // `transport` 看的是"这一轮会不会走 TLS"。地址不印全文，只印条数（一行 JSON 要能读）。
    let token_present = state::token_path(&cfg.data_dir).is_file();
    let paired = !st.paired_hub_pub.is_empty() && token_present;
    let hub_fp = st.hub_fp();
    let tls_fp = effective_tls_fp(cfg, st).unwrap_or_default();
    let endpoints = effective_endpoints(cfg, st);
    let transport = if !tls_fp.is_empty() {
        "tls"
    } else if !endpoints.is_empty() {
        "plaintext"
    } else {
        "none"
    };
    println!(
        "{{\"endpoint\":\"{}\",\"pending_turns\":{},\"last_rate_bps\":{},\"resume_count\":{},\"epoch\":{},\"seq\":{},\"last_error\":\"{}\",\"last_error_at_ms\":{},\"self_distill\":\"{}\",\"detached\":{},\"reconcile_confirmed_at_ms\":{},\"dr_backup_last_at_ms\":{},\"dr_backup_count\":{},\"local_turn_highwater_ms\":{},\"offline_elapsed_ms\":{},\"offline_since_ms\":{},\"center_seen_at_ms\":{},\"llm_key\":\"{}\",\"staging_rows\":{},\"paired\":{},\"hub_fp\":\"{}\",\"tls_fp\":\"{}\",\"transport\":\"{}\",\"endpoints\":{},\"rediscover_ok\":{},\"rediscover_miss\":{},\"last_rediscover_at_ms\":{}}}",
        st.current_endpoint,
        st.pending_turns,
        st.last_rate_bps,
        st.resume_count,
        st.current_manifest.epoch,
        st.current_manifest.seq,
        st.last_error,
        // 失败的时刻留着，成功一轮只清 `last_error`：两者一起清就分不出陈旧错误。
        st.last_error_at_ms,
        st.self_distill.as_str(),
        // `detached` 由状态枚举算；另四格是 B4 的自升/自备份读数（0 也印）。
        st.self_distill.is_detached(),
        st.reconcile_confirmed_at_ms,
        st.dr_backup_last_at_ms,
        st.dr_backup_count,
        st.local_turn_highwater_ms,
        st.offline_elapsed_ms,
        st.offline_since_ms,
        st.center_seen_at_ms,
        // 两格都是**现算**：`llm_key` 由配置与箱给（不是从状态文件抄的），
        // `staging_rows` 直接查暂存层的标记表（无层 ⇒ 0，是"没有新行"的真读数，不是未知）。
        selfdistill::live_key_reading(cfg, &cfg.data_dir),
        node::staging::new_rows(&cfg.data_dir).len(),
        paired,
        hub_fp,
        tls_fp,
        transport,
        endpoints.len(),
        // 三格是 S4d1 的重发现读数（0 也印：旧状态读出来就是 0）。
        st.rediscover_ok,
        st.rediscover_miss,
        st.last_rediscover_at_ms
    );
}

/// 生成只发生在**文件不存在**这一种情形。读不懂的钥匙是身份，不是空位——
/// 覆盖它等于换一台设备（中心那边的配对、审计、上行归属会一起换人）。
fn load_or_create_key(path: &Path) -> Result<DeviceKey, &'static str> {
    match std::fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let k = DeviceKey::generate().map_err(|_| "the device key cannot be generated")?;
            k.save(path).map_err(|_| "the device key file is unwritable")?;
            Ok(k)
        }
        Err(_) => Err("the device key file is unreadable — refusing to overwrite it"),
        Ok(_) => DeviceKey::load(path).map_err(|e| match e {
            rc_net::error::NetError::KeyFileMalformed => {
                "the device key file is malformed — refusing to overwrite it"
            }
            _ => "the device key file is unreadable — refusing to overwrite it",
        }),
    }
}

/// 时钟。**入口文件取钟是允许的**（判据 12a 的 allowlist 里有 `node/src/main.rs`）；
/// 库里一律不许取钟。
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 运维失败（退出码 3，与 hub / mcp / doctor 同义）。**一句话，不是栈**。
fn operational(msg: &str) -> ExitCode {
    let _ = std::io::stderr().write_all(format!("error: {msg}\n").as_bytes());
    ExitCode::from(3)
}

#[cfg(test)]
mod tests {
    use super::load_or_create_key;

    /// 生成钥匙只许发生在"文件不存在"这一种情形：**读不懂的钥匙是身份，不是空位**。
    /// 覆盖它等于换一台设备——中心那边配对、审计、上行归属全跟着换人。
    #[test]
    fn only_a_missing_key_file_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.key");
        let first = load_or_create_key(&missing).expect("a missing key is created");
        let second = load_or_create_key(&missing).expect("an existing key is read back");
        assert_eq!(first.device_id(), second.device_id(), "既有的钥匙不许被换掉");

        let broken = dir.path().join("broken.key");
        let junk = b"not a device key\n";
        std::fs::write(&broken, junk).unwrap();
        assert!(load_or_create_key(&broken).is_err(), "坏掉的钥匙必须响亮失败");
        assert_eq!(std::fs::read(&broken).unwrap(), junk, "坏掉的钥匙不许被覆盖");
    }
}
