// 记忆域的类型层与写入面。分工照文件分：
// `schema.rs` 声明列归属（守卫 11 的声明），`keys.rs` 声明跨模块句柄形状，
// `identity.rs` 是唯一一条内容寻址算法，`writers_{x,a,c,p}.rs` 是四个 actor 各一的写入口，
// `writers_fact.rs` 是 S2b-2a 的 fact 五表行写入口（fact/provenance/memory_mention/card/owner），
// S2c-1 起同文件多一条 `put_fact_rows_with_extras`（participants/value_json/place 三格附加），
// 旧入口委托它、行为不变；
// `episode`/`episode_fact` 的写入口（派发档 I-3a）住 `writers_x.rs` 本体、**不开 `writers_episode.rs`**：
// 判据 52③ 把 src 面的出处 INSERT 字面量钉成恰好 7 处 + 白名单四家（那七个字面量就是
// `INTO` 紧跟 `provenance` 与左括号的那几处，注释里不再复述它——复述一次就是把本文件
// 变成清单外的第八个家），而唯一合法的出处写法 `insert_derived_provenance` 是
// `writers_x.rs` 的私有 fn（母档 §2.1，理由不是风格）。
// 它的消费句柄是镜像卡 `card(kind='episode')`——`MemTable` 不扩（`keys.rs` 只有 `Anchor|Card`）。
// `reader.rs` 是读回，`orphans.rs` 是连带抑制前置扫描（段 0，唯一读者是 `hub::run_cycle_once`
// 与本 crate 的单测，不落 `CycleOut`），`owner.rs` 是资源归属的唯一实现点
// （S1b-C2b 吸收推导 / C3 写入继承 / C4 读闸共用；可见性上界与 scope 归一都在那里），
// `synth.rs` 只许测试引用（任务 11 的判据 12b/12c 守它）。
// `gate.rs` 是读闸的唯一实现点（S1b-C4a）：权限只在那里判，**不进** `reader::facts()` 的 SQL
// ——候选集的排序不许是权限的函数。
// `review.rs` 是 S3a-1 的双审队列存储面（入队/判定/列表/统计/清理），也是这两张表的唯一写点。
// `verdict.rs` 是 §7.3a 的逐条判决/证据附件表 `review_verdict`（两侧原判 + `final` 裁决，
// 每条草稿每侧一行）；它的 DDL 也只住在那里——这张表不在 `MEMORY_TABLES` 里（理由见该文件头）。
pub mod anomaly;
pub mod gate;
pub mod identity;
pub mod issues;
pub mod keys;
pub mod merge;
pub mod orphans;
pub mod owner;
pub mod reader;
pub mod redirect;
pub mod rebind;
pub mod recheck;
pub mod review;
pub mod reuse;
pub mod schema;
pub mod snapshot;
pub mod synth;
pub mod value_gate;
pub mod verdict;
pub mod writers_a;
pub mod writers_attr;
pub mod writers_c;
pub mod writers_fact;
pub mod writers_p;
pub mod writers_rows;
pub mod writers_state;
pub mod writers_v4;
pub mod writers_x;

pub use identity::{compute_fact_id, compute_mem_id};
pub use keys::{AnchorRole, AttentionKind, MemKey, MemTable, Scope, ScopeKind, Stance, Verdict};
pub use reader::{all_titles, attribute_owners, chain_links, directory_sql, edge_provenance, edges,
                 entity_directory, entity_ids_for_name, episode_members, episode_of_fact,
                 event_link_of, facts, fact_rows, lookup, provenance_chain, provenance_of,
                 resolve_surface, rows_in_scope, scope_of, unreviewed_rows, AttributeOwner,
                 ChainLink, EdgeFacts, EntityEntry, EventLink, FactRow, ProvenanceStep, RowFacts,
                 SurfaceHit};
pub use schema::{Actor, ColumnOwner, MEMORY_TABLES, OWNERSHIP};
// 波二刀 A：复用反馈的写口/读口与值域闭集（`ReuseOutcome` 是 outcome 的唯一家）。
// 刀 B 起再加消费两臂的读口：rank 臂的批量聚合（`reuse_ledger` + `WORKED_TIER_MIN`）与
// 审计臂的程序判定/入队（`audit_candidates` / `ingest_audit`）。
pub use reuse::{
    audit_candidates, ingest_audit, recent_events, record_reuse, reuse_counts, reuse_ledger,
    ReuseAuditCandidate, ReuseAuditItem, ReuseAuditOut, ReuseCounts, ReuseError, ReuseEvent,
    ReuseOutcome, AUDIT_OUTCOMES, WORKED_TIER_MIN,
};
pub use review::{
    decisions, enqueue, get, has_stage, list, prune, record_decision, stats, Decider, Decision,
    DecisionRow, EnqueueOutcome, Item, ItemMeta, NewItem, ReviewStage, ReviewState, ReviewStats,
    ReviewVerdict,
};
pub use verdict::{
    item_verdicts, item_verdicts_filtered, replace_item_verdicts, EvidenceState, NewVerdict,
    VerdictRow, VerdictSide,
};
pub use writers_a::attest;
pub use writers_attr::{
    attribute_role, compute_attribute_id, ensure_environment_anchor, put_attribute_rows,
    AttrWriteOutcome, ATTRIBUTE_KEYS, ATTR_BILIBILI_ACCOUNT, ATTR_BRAND, ATTR_DEVICE_ID,
    ATTR_HOSTNAME, ATTR_MODE_STATED, ATTR_OS_USER, ATTR_QODER_ACCOUNT, ATTR_VALUE_TYPE_STRING,
    ENV_ANCHOR_KIND,
};
pub use writers_c::{promote_verdict, resolve_retirement, retire, retire_event_link,
                    revert_verdict, set_verdict, set_verdict_from, supersede,
                    VERDICT_SOURCE_AUDIT, VERDICT_SOURCE_REVIEW_APPLY};
pub use writers_fact::{put_fact_rows, put_fact_rows_with_extras, FactExtras, FactWriteItem,
                       FactWriteOutcome, FACT_CATEGORIES, FACT_MODE_STATED};
pub use writers_state::{
    put_state_rows, StateWriteItem, StateWriteOutcome, STATE_DIMENSIONS, STATE_MEM_TABLE,
    STATE_MODE_EXPRESSED, STATE_MODE_INFERRED,
};
pub use writers_p::{bump_cooldown, clear_expired, COOLDOWN_MS};
pub use writers_v4::{ensure_agent_anchor, write_a1, write_a1_with_owner, A1Anchor, A1Batch,
                     A1Issue, A1Mention, A1Outcome, A1Unresolved};
pub use writers_x::{
    check_alias, put_assertion, put_batch, put_batch_with_owner, put_card, put_chains, put_edges,
    put_entity, put_episodes, put_event, put_mentions, put_timeline_rows, BatchCtx, ChainContent,
    EdgeContent, EntityContent, EpisodeContent, EpisodeWriteOutcome, FactContent, MentionContent,
    MentionOutcome, PRONOUNS, PRONOUNS_ASCII, TimelineContent, WriteItem,
};

/// 一条出处：X 写内容列必带它（§2 规则 2，无例外）。
/// `turn_uid` 是会话库那一侧的身份值，`anchor_role` 是那一轮的角色——强锚判定（V6）的载体。
/// 这里只带 uid 不带正文：跨库不许有第二个数据源（守卫 1），也不许有正文（守卫 12）。
#[derive(Debug, Clone)]
pub struct Provenance {
    pub turn_uid: String,
    pub anchor_role: AnchorRole,
}

/// 写入面的错误。载荷只许是键、表名与 slug：它是运维输出的上游，
/// 一条把文件路径或记忆正文带进 Display 的错误变体就是守卫 12 的破口。
/// `Sql` 那一支在构造点过一遍 `sqlite_text`（见下），不是原样转述 rusqlite。
#[derive(Debug, thiserror::Error)]
pub enum MemError {
    #[error("memory row {0} was written without provenance")]
    ProvenanceMissing(&'static str),
    #[error("handle is a read-only replica, no writer accepts it")]
    ReadOnly,
    #[error("positive attestation needs an anchor turn_uid that is a human-turn provenance of this row")]
    AnchorNotHuman,
    #[error("alias {0} is a pronoun and can never be an alias (guard 10)")]
    AliasIsPronoun(String),
    #[error("no such memory row: {}", .0.key_string())]
    NotFound(MemKey),
    #[error("content column is empty after normalization")]
    ContentEmpty,
    #[error("a1 input has an invalid {0}")]
    BadA1Field(&'static str),
    #[error("a1 row references an anchor ref that is not in this batch")]
    UnknownAnchorRef,
    #[error("sqlite: {0}")]
    Sql(String),
}

impl From<rusqlite::Error> for MemError {
    fn from(e: rusqlite::Error) -> Self {
        MemError::Sql(sqlite_text(&e))
    }
}

/// 守卫 12 在构造点的消毒。rusqlite 的 `Display` 里有两支会把不该外带的东西写进消息：
/// `InvalidPath(p)` 直接打印文件路径，`SqlInputError { sql, .. }` 打印整条 SQL（多行、含列名），
/// 而 sqlite 自己的 errmsg 在 I/O 类错误上可能带库文件名。这里取三道：
/// ① 只留第一行；② 截到 120 字符；③ 文本里出现 `/`、`\` 或 `.db` 就整段换成占位串。
/// 代价明写：极端情况下少一点排障线索（真要定位仍然可以看 `StoreError` 那一侧或开 sqlite 日志），
/// 换到的是"任何一条 MemError 渲染出去都不可能吐出一个路径或一段正文"。
pub(crate) fn sqlite_text(e: &rusqlite::Error) -> String {
    let head = e.to_string().lines().next().unwrap_or_default().to_string();
    let cut: String = head.chars().take(120).collect();
    if cut.contains('/') || cut.contains('\\') || cut.to_lowercase().contains(".db") {
        return "error detail redacted (path-like text)".to_string();
    }
    cut
}
