//! 节点的连通性读数：当前路径 / 速率 / 续传次数。
//!
//! 形状由 `node::state` 独占——**这里只读不算**。
//! 机器上不是节点（没有节点状态文件）⇒ `None`，**不是**一份零值的读数：
//! "这台机器不是节点"与"节点连不上"是两种后果。

use node::state::{self, SyncState};
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Connectivity {
    /// 当前路名（不是地址）。
    pub endpoint: String,
    pub last_rate_bps: u64,
    pub resume_count: u64,
    pub pending_turns: i64,
    pub last_push_at_ms: i64,
    pub last_pull_at_ms: i64,
    /// 当前那份副本的清单时刻。
    pub as_of_ms: i64,
    /// B2b-3：离线自蒸的状态机现值（`node::state::SelfDistill::as_str()` 的原样串）。
    pub self_distill: String,
    /// B4：第 5 天的两态（`detached_distilling` / `detached_promoted`）。由状态枚举算。
    pub detached: bool,
    /// 中心确认收下回并批次的时刻；**0 = 未确认**（自备份一份都不删）。
    pub reconcile_confirmed_at_ms: i64,
    /// 最近一次自备份的时刻（0 = 还没备份过）。
    pub dr_backup_last_at_ms: i64,
    /// 已建立的自备份份数。
    pub dr_backup_count: u32,
    /// 本机转录层到达钟的高水位。
    pub local_turn_highwater_ms: i64,
    /// 单调累计的离线时长。
    pub offline_elapsed_ms: i64,
    /// 暂存层标记表的行数。`0` 是**真读数**（层不在或没有新行，`new_rows` 对无层返回空），不是未知。
    pub staging_rows: usize,
    /// 这台机器报告过的钥匙面（三值之一）。`None` = 还没报告过 ⇒ doctor 那一格不出现。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_key: Option<String>,
    /// 最后一次同步失败的封闭码（`""` = 没失败过或已被一次成功清掉）。
    pub last_error: String,
    /// 那个失败发生的时刻。与 `last_error` 分开：一个说"现不现"，一个说"什么时候"。
    pub last_error_at_ms: i64,
    /// S4d1：改地址重试真跑通的次数。
    pub rediscover_ok: u64,
    /// S4d1：问了但没找到（或新地址没跑通）的次数。
    pub rediscover_miss: u64,
    /// 状态里那台中心的短名（`hub_fingerprint`）；未配对 ⇒ 空。
    pub hub_fp: String,
    /// 状态里的钉死证书指纹；没有 ⇒ 空。**配置文件里的那一枚不在这里**：
    /// doctor 只读状态，不读 `node.toml`。
    pub tls_fp: String,
    /// `tls` / `plaintext` / `none`，由上面两格与状态端点算。
    pub transport: String,
}

pub fn read_connectivity(data_dir: &Path) -> Option<Connectivity> {
    let st: SyncState = state::load(&state::state_path(data_dir)).ok()??;
    // 三格由状态现算：`hub_fp` 是"我钉的是哪一台"，`transport` 是"会不会走 TLS"。
    // 旧状态文件缺这些字段时读出来就是空/0——**空表示"不知道"，不是"没有"**，
    // 所以 status 的 `transport` 在状态这一侧只能看见票带来的那一半地址。
    let hub_fp = st.hub_fp();
    let tls_fp = st.paired_tls_fp.clone().unwrap_or_default();
    let transport = if !tls_fp.is_empty() {
        "tls"
    } else if !st.paired_endpoints.is_empty() {
        "plaintext"
    } else {
        "none"
    };
    Some(Connectivity {
        endpoint: st.current_endpoint,
        last_rate_bps: st.last_rate_bps,
        resume_count: st.resume_count,
        pending_turns: st.pending_turns,
        last_push_at_ms: st.last_push_at_ms,
        last_pull_at_ms: st.last_pull_at_ms,
        as_of_ms: st.current_manifest.created_at_ms,
        self_distill: st.self_distill.as_str().to_string(),
        detached: st.self_distill.is_detached(),
        reconcile_confirmed_at_ms: st.reconcile_confirmed_at_ms,
        dr_backup_last_at_ms: st.dr_backup_last_at_ms,
        dr_backup_count: st.dr_backup_count,
        local_turn_highwater_ms: st.local_turn_highwater_ms,
        offline_elapsed_ms: st.offline_elapsed_ms,
        staging_rows: node::staging::new_rows(data_dir).len(),
        llm_key: st.llm_key.clone(),
        last_error: st.last_error,
        last_error_at_ms: st.last_error_at_ms,
        rediscover_ok: st.rediscover_ok,
        rediscover_miss: st.rediscover_miss,
        hub_fp,
        tls_fp,
        transport: transport.to_string(),
    })
}
