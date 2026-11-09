//! 写入面工具（`remember` / `vote` / `report` / `status`）与副本身份读数 `source_of`。
//! `remember` 的落库**经 `sinks::TurnSink`**（调用方实现），所以本档只断缝两侧的形状；
//! 真实落库由任务 10 在真子进程 + 真会话库上断（缝的两侧各测一半）。

use crate::error::ToolError;
use crate::sinks::{CycleRead, TurnSink};
use rc_memory::keys::{MemKey, MemTable};
use rc_memory::reader;
use rc_memory::Stance;
use rc_store::{MemoryHandle, TranscriptHandle};
use serde_json::json;

/// 单次 `remember` 的正文上限。**常量不是配置项**（与 `rc_search::FLOOR` 同一条纪律）：
/// 一个可配的数会让人以为"调大就行"，而这里的真实约束是"一条结论加上它的理由"的量级。
/// 单位是**字符**不是字节（守卫 8 那一族：按字节算中文上限会凭空小三倍）。
/// 值待阶段 5 用真实转录分布复核；复核前它是刻度不是猜测——拒收行为不依赖具体数值。
pub const MAX_REMEMBER_CHARS: usize = 4000;

/// `status` 目录段的截断上限。**住这个文件**（协议面的形状），而测试走
/// `tools` 的模块路径而不是根导出——给一个常数加第二个入口就是下一次漂移的地方。
/// 20 不是量出来的而是协议选择：`brief` 那一行 JSON 是 stdout 上的，宿主把它塞进上下文，
/// 一行里塞 100 个实体就是在花别人的窗口买一个"我知道几个"的读数（G7 的截断要能自证，
/// 所以配套的是 `entities_unlisted` 而不是一个更大的数）。
pub const MAX_STATUS_ENTITIES: usize = 20;

/// 一次提示最多列几条（§6.5 P4 的"至多 3 条"）。**常数、不进根导出、不上 wire**，
/// 与任务 11 那个目录上限同一条纪律：上限的家只在定义它的那一个文件（这里不重抄它的名字——本文件有条判据在数它）。
pub const MAX_SIMILAR_NOTES: usize = 3;

/// `recollect_report` 的两个输入刻度。**常量不是配置项**（与 `MAX_REMEMBER_CHARS` 同一条纪律）。
/// 类别是给审核分诊用的短标签；说明是"这一条为什么有误"的自由文本——一条报告的量级，
/// 不是一份文档。单位都是**字符**（守卫 8 那一族）。
pub const MAX_REPORT_CATEGORY_CHARS: usize = 64;
pub const MAX_REPORT_NOTE_CHARS: usize = 2000;

/// 无输入不可构造（守卫 5 在写入面的形状）：`new` 是唯一的门，字段私有。
#[derive(Debug, Clone)]
pub struct RememberRequest {
    body: String,
}

impl RememberRequest {
    pub fn new(raw: &str) -> Result<Self, ToolError> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(ToolError::validation(
                "empty_remember",
                "recollect_remember needs content; got none",
            ));
        }
        if trimmed.chars().count() > MAX_REMEMBER_CHARS {
            // 拒收并给出可执行的下一步，**不截断**：截断等于系统替调用方改写了它要说的那句话。
            // hint 里不许写数字（`tools_shape.rs` 有一条断 hint 不含数字）：上限的家只有常量这一处。
            return Err(ToolError::validation(
                "remember_too_long",
                "split it into separate remembers; see capabilities.max_remember_chars",
            ));
        }
        Ok(RememberRequest { body: trimmed.to_string() })
    }
    pub fn body(&self) -> &str {
        &self.body
    }
}

/// `status` 的可选输入。**任务 11 在这里加 `entity`**，所以 `Default` 是派生的而不是手写的。
/// 调用点的形状：没问任何格子的写 `StatusOptions::default()`，问了 `turn` 的写
/// `StatusOptions { turn, entity: None, ..Default::default() }`（结构体更新语法，clippy
/// `useless_struct_update` 不再当拒）。G19 加 `view`：`Counts` 是轻量读数面。
#[derive(Debug, Default, Clone, Copy)]
pub struct StatusOptions<'a> {
    pub turn: Option<&'a str>,
    /// P3 目录的过滤词：实体的标题**或任一别名**（守卫 10 的入口）。与 `turn` 互斥（§2.14）；
    /// 与 `view=counts` 也互斥——counts 面没有目录，静默丢掉过滤条件就是撒谎。
    pub entity: Option<&'a str>,
    /// G19 刀 1：缺省 [`StatusView::Full`] 与既有大对象**逐字节相同**（既有测试不许翻红）；
    /// `Counts` 只回计数格。闭集校验的家在 mcp 的 `view` 形参（schema 只是声明，本阶段不执行），
    /// 本面收到的是已解析的枚举——坏值造不成第二次分型。
    pub view: StatusView,
}

/// `recollect_status` 的两个视图（G19 刀 1）。DSH 实测缺省面 ~4.6KB，大头是工具能力声明
/// （工具名清单 + 沉默格清单）与实体目录——那是"目录页"，不是"计数页"。`Counts` 回
/// 各表行数、滞后、attention 等计数格，不带那两块。变体只有两个，`Full` 是缺省：
/// 这是 C2 演进规则里的"加一个可选面"，不是改默认行为，`SCHEMA_VERSION` 不动。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum StatusView {
    /// 既有的大对象，一字不动。
    #[default]
    Full,
    /// 计数页：无 `capabilities`（工具名清单的家）、无实体目录三格。
    Counts,
}

impl StatusView {
    /// slug 的唯一解析点。**不 trim、不做大小写折叠**（与 `VoteStance::from_slug` 同一条纪律：
    /// 精确匹配，宿主的格式化是宿主的 bug）。
    pub fn from_slug(raw: &str) -> Option<StatusView> {
        match raw {
            "full" => Some(StatusView::Full),
            "counts" => Some(StatusView::Counts),
            _ => None,
        }
    }
}

/// `turn_uid` 的形状门：64 位**小写** hex。它住工具面而不是缝里：形状是输入校验，
/// 而 `rc-contract::compute_turn_uid` 生产它、不校验它。与 `vote` 的 `MemKey::parse` 同族——
/// **先分形状（Validation）、再问存在（Empty）**，两个分型各一条（P-62 的第三条腿）。
/// 只认小写：`"F".repeat(64)` 那样的输入是宿主的 bug，不是我们的规范化职责
/// （`VoteStance::from_slug` 的"精确匹配"是同一条纪律）。
/// A9 起 `pub`：`mcp` 的 `recollect_report` 臂要用它分"行句柄 / 裸 turn_uid"两条路，
/// 轮的存在性检查要 transcript 柄（守卫 6 不许 rc-agent 碰 rc-ledger），所以分型留在门面。
pub fn is_turn_uid(raw: &str) -> bool {
    raw.len() == 64 && raw.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// P2 三格的**唯一生产者**（§2.14："`remember` 与 `status(turn=)` 共用同一个生产者"这句
/// 在代码上必须是真的）。它只做两件事——缝的一次调用 + 一次序列化——**不做分型**：
/// `None` 在两个面上是两个不同的事实（P-62），所以返回 `Option`，由调用方各自映射。
/// 序列化只做一次、键名只有一个家（`TurnExtraction::fields()`），两半合起来才是"同一个查询面"。
fn extraction_readout(
    read: &dyn CycleRead,
    turn_uid: &str,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, ToolError> {
    Ok(read.turn_extraction(turn_uid)?.map(|t| t.fields()))
}

/// 写一条转录，并**经读缝 `CycleRead` 反查它的抽取态**（任务 10）。
/// 三格（`extract_state` / `reason` / `eta_turns`）摊平在顶层，**不再**取自 `capabilities()`
/// （P-45：声明与读数同源时，水位库里的四态在写入面上永远读不出）。
/// 为什么现在就要有 `remember`：`vote` 的正面表态锚必须指向一条**已存在**的轮，
/// 而没有写入面就没有可锚的东西——这两个工具是一起来的，拆不开。
///
/// P4（任务 12）再加两格可空的相似提示（`similar_n` / `similar`）：`null` = 这次没比，
/// `0` / `[]` = 比过没有像的。读缝只与 `&MemoryHandle` 共享借用，与 `TurnSink` 的
/// `borrow_mut` 在同一次调用里**顺序**发生（P-48 的那条坑在 `mcp` 那一侧兑现）。
pub fn remember(
    sink: &dyn TurnSink,
    read: &dyn CycleRead,
    m: &MemoryHandle,
    viewer: Option<&rc_memory::gate::Viewer>,
    req: &RememberRequest,
    session_key: &str,
    now_ms: i64,
) -> Result<crate::ToolJson, ToolError> {
    remember_inner(sink, read, Some(m), viewer, req, session_key, now_ms)
}

/// 节点还没有副本时的同一条写路径：写本机转录层，只是没有可比的记忆库。
///
/// 相似度两格与 `similar_hint` 的降级形状相同（`null`，不是 `0`）——"这次没比"与
/// "比过、没有像的"是两种事实。其余三格（抽取态、原因、队列位置）照常经读缝反查。
///
/// **为什么必须有这一条**：副本要等中心先有快照才下得来，而节点的轮要经转录层上行。
/// 若"没有副本"连 `remember` 都拒绝，一台新节点就永远攒不出第一行可上行的内容。
pub fn remember_local(
    sink: &dyn TurnSink,
    read: &dyn CycleRead,
    req: &RememberRequest,
    session_key: &str,
    now_ms: i64,
) -> Result<crate::ToolJson, ToolError> {
    // 没有记忆库 ⇒ 没有相似度读数 ⇒ viewer 无处可用（`remember_inner` 只在
    // `m` 在场时读它）。
    remember_inner(sink, read, None, None, req, session_key, now_ms)
}

fn remember_inner(
    sink: &dyn TurnSink,
    read: &dyn CycleRead,
    m: Option<&MemoryHandle>,
    viewer: Option<&rc_memory::gate::Viewer>,
    req: &RememberRequest,
    session_key: &str,
    now_ms: i64,
) -> Result<crate::ToolJson, ToolError> {
    if session_key.trim().is_empty() {
        // 会话键来自宿主，是系统边界上的输入，所以这里校验（内部代码不校验）。
        return Err(ToolError::validation(
            "empty_session",
            "recollect_remember needs the host's session key; got none",
        ));
    }
    let turn_uid = sink.append_agent_turn(session_key, req.body(), now_ms)?;
    // P4 的相似度提示**必须在写之后**算：它读的是记忆库，与刚写的转录无关，但顺序读起来
    // 与下面的 `extraction_readout` 是同一条纪律——"先写完，再读"（任务 10 Step 3 的原话）。
    // 它也不许 `?`：任何 `rank` 失败都只折成两格 `null`（P-61）。
    let similar = match m {
        // W37-A1（G16 similar 旁路修复）：相似提示**带读者身份**过读闸——没有 viewer
        // 的 rank 是"库里有什么就报什么"，会把别的 agent 的 private 行的 id+标题
        // 泄进 remember 回执。`None` viewer ⇒ 不过闸（旧调用形状，与 rank 同一条约定）。
        Some(m) => similar_hint(m, viewer, now_ms, req.body()),
        None => similar_none(),
    };
    // **顺序是本任务的整个契约**：先写完、再读。反过来做时刚写的那一轮还不在转录里，
    // `turn_origin` 结构上只能返回 `None`，于是每一次 remember 都撞 `turn_unreadable`——
    // 而那是一条看起来像存储故障的红（P-61 同族：不许给一条成功的写新增一种失败方式）。
    // 同一个 `None` 在这里读成 Unavailable（刚写完自己那句话却读不到 = 库不一致，P-62）。
    let fields = extraction_readout(read, &turn_uid)?
        .ok_or_else(|| ToolError::unavailable("turn_unreadable", "run: doctor"))?;
    let mut out = json!({
        "schema_version": crate::SCHEMA_VERSION,
        "turn_uid": turn_uid,
        "accepted_chars": req.body().chars().count(),
    });
    // 三格摊平在顶层 + P4 的两格可空（顺序由 `serde_json::Map` 的 BTreeMap 性质保证为字典序）。
    let obj = out.as_object_mut().expect("json! 的字面量是对象");
    for (k, v) in fields.into_iter().chain(similar) { obj.insert(k, v); }
    Ok(crate::ToolJson::from_value(out))
}

/// P4 的两格。**私有的理由不是整洁**：`null` 与 `0` 的分界是这一族唯一一处需要判的地方，
/// 给它开一个 `pub` 入口就是给"什么时候算没比过"造第二个决定点（P-59 的根）。
///
/// 三条降级路（`NotBuilt`／`NormMismatch`／`NoQueryTokens`）在 `rank` 的 `Err` 上是
/// **同一支 `Err(_)`**——这里**不许**按错误分型（P-61）：`remember` 的成功与否只由"那一行
/// 有没有写下去"决定，相似度是附赠读数。`NormMismatch` 在这一支上没有专属用例，理由见
/// Step 1 纪律 4（那张 marker 表的名字住在 `rc-index` 的 `pub(crate) mod shadow` 里）。
fn similar_none() -> Vec<(String, serde_json::Value)> {
    vec![("similar_n".to_string(), serde_json::Value::Null),
         ("similar".to_string(), serde_json::Value::Null)]
}

fn similar_hint(
    m: &MemoryHandle,
    viewer: Option<&rc_memory::gate::Viewer>,
    now_ms: i64,
    body: &str,
) -> Vec<(String, serde_json::Value)> {
    let Ok(ranked) = rc_search::rank(m, viewer, now_ms, body) else { return similar_none(); };
    // 过滤三件事，顺序无关（`rank` 已经是全序，这里不重排 = 第二把尺子不存在）：
    // ① 过线（档位谓词只有 rc-search 那一个，名字就在下面那行调用里；两个相关名字下面各有判据在数，这里不重抄）；
    // ② 不是实体行（P-60）；③ 还活着（P-67，谓词的家在 `rc-memory`，这里只调一次）。
    let hits: Vec<&rc_search::Scored> = ranked
        .iter()
        .filter(|s| s.above_floor() && s.key.table != MemTable::Anchor && s.facts.alive())
        .collect();
    let notes = hits
        .iter()
        .take(MAX_SIMILAR_NOTES)
        .map(|s| serde_json::json!({"id": s.key.key_string(), "title": s.facts.title}))
        .collect();
    vec![("similar_n".to_string(), serde_json::json!(hits.len())),
         ("similar".to_string(), serde_json::Value::Array(notes))]
}

/// agent 面能构造的四种表态。**它没有 `delete`**（架构 §3："agent 面永远构造不出删除这个动作"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoteStance {
    Confirmed,
    Corroborated,
    Rejected,
    Retired,
}

impl VoteStance {
    pub const ALL: [VoteStance; 4] =
        [VoteStance::Confirmed, VoteStance::Corroborated, VoteStance::Rejected, VoteStance::Retired];

    pub fn slug(self) -> &'static str {
        match self {
            VoteStance::Confirmed => "confirmed",
            VoteStance::Corroborated => "corroborated",
            VoteStance::Rejected => "rejected",
            VoteStance::Retired => "retired",
        }
    }

    /// `slug()` 的逆。**不 trim、不做大小写折叠**：这是四个常量的精确匹配。
    /// 它存在的唯一理由是 `mcp` 拿到的是字符串而变体名不许上 wire；
    /// 任何"顺手宽容一点"都会让 `bad_stance` 那一型在两个门面上都测不到。
    pub fn from_slug(raw: &str) -> Option<VoteStance> {
        VoteStance::ALL.into_iter().find(|s| s.slug() == raw)
    }

    fn to_stance(self) -> Stance {
        match self {
            VoteStance::Confirmed => Stance::Confirmed,
            VoteStance::Corroborated => Stance::Corroborated,
            VoteStance::Rejected => Stance::Rejected,
            VoteStance::Retired => Stance::Retired,
        }
    }

    /// **这条不对称是有意的**：`Rejected` 经 `facts.rejected`（它的读者是 `decide`）立刻进闸门；
    /// `Retired` 只落表态行、不落抑制账（那是 `C` 的 `retire`），所以它在阶段 1 **不改变可见性**。
    /// 把它做成"看起来会删除"才是最坏的形状：agent 会以为它不再被提起，而它下一轮照样出现。
    /// 所以这个字段是**声明**，`row_now` 是**读数**，两个都给，让调用方读到真相而不是猜测。
    pub fn suppresses(self) -> bool {
        matches!(self, VoteStance::Rejected)
    }
}

/// 记一次表态。**先认行再写**（架构 §6 消费侧②：悬空表态比没有表态更坏），
/// 而 `attest` 自己只校验锚不校验行存在——所以这一道门必须在工具面这里，不能推给下层。
///
/// 这是**不带读者**的旧入口（`viewer = None`）：与 [`vote_with_viewer`] 是同一段实现，
/// 只是不过读闸。它留着是因为中心 CLI / 既有测试的调用形状不该被一次接线改写；
/// **生产读面必须走带 `viewer` 的那一支**（S1b-C4b1：被拒与"行不存在"同形）。
pub fn vote(
    m: &MemoryHandle,
    target: &str,
    stance: VoteStance,
    anchor: Option<&str>,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<crate::ToolJson, ToolError> {
    vote_with_viewer(m, None, target, stance, anchor, reason, now_ms)
}

/// 带读闸的表态（S1b-C4b1，设计 §4.2）：`viewer` 是 agent 面的读者身份。
///
/// **句柄校验过闸，写后回读不过闸**：`lookup_visible` 返回 `None` 时走既有的
/// `empty:row_absent` 分支——被拒与"这一行不存在"同形，库里有几条、是谁的，一个字都不说
/// （存在性本身就是要藏起来的那件事）。确认可见之后 `attest` 才落表态，随后那一行
/// 必然仍对同一位读者可见（owner 会变吗？不会：`attest` 只写表态行，不碰归属），
/// 所以回读用普通 `lookup`——多一次裁决只会把"刚写完自己却读不到"变成一种新的失败方式。
pub fn vote_with_viewer(
    m: &MemoryHandle,
    viewer: Option<&rc_memory::gate::Viewer>,
    target: &str,
    stance: VoteStance,
    anchor: Option<&str>,
    reason: Option<&str>,
    now_ms: i64,
) -> Result<crate::ToolJson, ToolError> {
    if m.is_read_only() {
        // 架构 §6："离线写路径一律响亮拒绝并给出可诊断原因，绝不在本地排队等上行"。
        // 这一支不许交给下层的 ReadOnly 去代理：在**打开写之前**就拒，
        // 因为让 SQLite 去撞只读连接会把"资格问题"报成"SQL 出错" = 分型族里 Authorization
        // 冒充 Unavailable，而那正是 §11 D5 点名的"连不上被读成密码错了"。
        return Err(ToolError::authorization(
            "replica_read_only",
            "nothing is queued locally; re-run against the center",
        ));
    }
    let key = MemKey::parse(target).ok_or_else(|| {
        ToolError::validation(
            "bad_handle",
            "expected '<table>:<64 hex>'; a malformed handle is never guessed into a row",
        )
    })?;
    // 读闸只走 `rc_memory::gate` 的单一实现（设计 §4.1）：这里不写第二套规则。
    if rc_memory::gate::lookup_visible(m, viewer, now_ms, &key)
        .map_err(ToolError::from)?.is_none()
    {
        return Err(ToolError::empty(
            "row_absent",
            "the handle does not resolve; nothing was written",
        ));
    }
    rc_memory::attest(m, &key, stance.to_stance(), anchor, reason, now_ms)
        .map_err(ToolError::from)?;
    let after = reader::lookup(m, &key).map_err(ToolError::from)?.ok_or_else(|| {
        ToolError::unavailable("row_changed_mid_call", "run: hub index status --data-dir <dir>")
    })?;
    let d = crate::recall::decide_no_query(&after, now_ms);
    Ok(crate::ToolJson::from_value(json!({
        "schema_version": crate::SCHEMA_VERSION,
        "id": key.key_string(),
        "stance": stance.slug(),
        "suppresses": stance.suppresses(),
        "row_now": {
            "form": serde_json::to_value(d.form).expect("Form has derived Serialize"),
            "silence_reason": d.cell.map(|c| c.slug()),
        },
    })))
}

/// 副本必须**响亮**自报身份（架构 §6 那张表的右栏）。
/// 它住 `rc-agent` 而不是两个二进制里，因为同一个工具在两个面上的返回必须形状相同（守卫 3 的另一个面）。
pub fn source_of(m: &MemoryHandle) -> &'static str {
    if m.is_read_only() {
        "replica"
    } else {
        "live"
    }
}

/// A9（W34 2026-11-07）：反馈闭环的最小入口——消费者（人或 agent）报告某条记忆有误。
/// 本刀**只做入队**：构造 `review_item`（`source='report'`、`pass_id='report'`）经
/// [`rc_memory::review::enqueue`] 进既有审核队列，之后走既有的人工判定链
/// （`hub review list/show/decide`）；周期复核与自动 supersede 不在这一刀里。
///
/// 为什么 `batch_key` 恒空：报告没有"批次"语义，不参与 H1c 的在途顶替；
/// 同目标同载荷的重复报告由**载荷去重**（`(pass_id, family, payload_sha256)` 唯一索引）
/// 折成 `fresh=false`，这正是既有去重链的用法，不新造。
///
/// 身份三格：`source` 列 = `'report'` 是队列侧的辨识位（`NewItem` 本来就有 source 格，
/// A9 只把闭集放宽一词——比在 payload 里造约定字段侵入更小、SQL 可直接 `WHERE source='report'`）；
/// 报告人住 `owner_json` 的 `{"reporter": …}`（owner_json 参与载荷指纹，换报告人即换 item，
/// 同一报告人重报同一件事才去重）；设备号住 `device_id`（写入口的机器主语，与转录侧同源）。
///
/// 目标两种形状：`<table>:<64 hex>` 行句柄（与 `vote` 同一道形状门 + 同一道读闸——
/// **先认行再写**，悬空报告比没有报告更坏）与裸 64 位小写 hex 的 `turn_uid`。
/// 轮的**存在性**检查在门面（`mcp` 握着转录柄；守卫 6），这里只查形状。
// 8 个形参超 clippy 默认帽：与 `rc-distill/src/review/mod.rs` 的 `decision` 同一条放行
// ——这 8 格各有家（柄/身份/输入/时钟），并成一个结构体只会让门面的调用点多一层搬运。
#[allow(clippy::too_many_arguments)]
pub fn report(
    m: &MemoryHandle,
    viewer: Option<&rc_memory::gate::Viewer>,
    reporter: &str,
    device_id: &str,
    target: &str,
    category: &str,
    note: Option<&str>,
    now_ms: i64,
) -> Result<crate::ToolJson, ToolError> {
    if m.is_read_only() {
        // 与 `vote_with_viewer` 同一支：只读副本上的写要在**打开写之前**就拒，
        // 不让 SQLite 去撞只读连接把资格问题报成 SQL 出错。
        return Err(ToolError::authorization(
            "replica_read_only",
            "nothing is queued locally; re-run against the center",
        ));
    }
    let category = category.trim();
    if category.is_empty() {
        return Err(ToolError::validation(
            "empty_category",
            "recollect_report needs a short category; got none",
        ));
    }
    if category.chars().count() > MAX_REPORT_CATEGORY_CHARS {
        return Err(ToolError::validation(
            "category_too_long",
            "category must be a short tag",
        ));
    }
    let note = note.map(str::trim).filter(|n| !n.is_empty());
    if let Some(n) = note {
        if n.chars().count() > MAX_REPORT_NOTE_CHARS {
            return Err(ToolError::validation(
                "note_too_long",
                "shorten the note; one report is one review item",
            ));
        }
    }
    if !is_turn_uid(target) {
        let key = MemKey::parse(target).ok_or_else(|| {
            ToolError::validation(
                "bad_handle",
                "expected '<table>:<64 hex>' or a 64-hex turn_uid; a malformed target is never guessed into a row",
            )
        })?;
        // 读闸只走 `rc_memory::gate` 的单一实现（与 `vote_with_viewer` 同一条纪律）：
        // 被拒与"行不存在"同形，存在性不外泄。
        if rc_memory::gate::lookup_visible(m, viewer, now_ms, &key)
            .map_err(ToolError::from)?
            .is_none()
        {
            return Err(ToolError::empty(
                "row_absent",
                "the handle does not resolve; nothing was queued",
            ));
        }
    }
    // 载荷**确定性**（无时钟、无随机量）：重复报同一目标+同类别+同说明必须算出同一份
    // payload_json，载荷去重才有 `fresh=false` 可言。键序由 serde_json 的 BTreeMap 定，
    // 与书写顺序无关。
    let payload = json!({
        "schema": "report.v1",
        "target": target,
        "category": category,
        "note": note,
    });
    let item = rc_memory::review::NewItem {
        source: "report".to_string(),
        pass_id: "report".to_string(),
        family: String::new(),
        session_key: String::new(),
        device_id: device_id.to_string(),
        // 句柄照原样进 `turn_uids_json`：这一列在报告项上只是"目标可追溯"的落点，
        // 行句柄不是 turn_uid——真正的结构化目标在 payload 里，人审时读 payload。
        turn_uids_json: json!([target]).to_string(),
        owner_json: json!({ "reporter": reporter }).to_string(),
        payload_json: payload.to_string(),
        batch_key: String::new(),
    };
    let out = rc_memory::review::enqueue(m, &item, now_ms).map_err(ToolError::from)?;
    Ok(crate::ToolJson::from_value(json!({
        "schema_version": crate::SCHEMA_VERSION,
        "target": target,
        "category": category,
        "review_item_id": out.id,
        "fresh": out.fresh,
        "state": out.state.slug(),
    })))
}

/// 波二刀 A（G1 复用反馈）：`recollect_reuse` 的实现缝——"这张卡帮没帮上忙"。
///
/// **与 [`vote_with_viewer`] 分轴不分路**：句柄形状、读闸、"先认行再写"三道门逐字同律；
/// 闭集（五档）、载体（`card_reuse_event`）与回执形状各是各的。两轴语义不同源
/// （表态=我认为真假，复用=它帮没帮上忙），所以不复用 vote 的闭集，也不把复用塞进
/// `attestation` 表。
///
/// **append-only**：写口只追加（[`rc_memory::reuse::record_reuse`]），同一卡同一 outcome
/// 两次调用 = 两行——"用过两次、两次都成"正是刀 B rank 臂要读的那一格。
///
/// 闭集在这一层与写口各查一次是有意的：这一层给的是"改输入"这一下一步（`bad_outcome`），
/// 写口那一层是最后一堵墙（测试两层各钉一次）。
pub fn reuse_with_viewer(
    m: &MemoryHandle,
    viewer: Option<&rc_memory::gate::Viewer>,
    handle: &str,
    outcome: &str,
    note: Option<&str>,
    by_agent: &str,
    now_ms: i64,
) -> Result<crate::ToolJson, ToolError> {
    if m.is_read_only() {
        // 与 `vote_with_viewer` 同一支：只读副本在**打开写之前**就拒。
        return Err(ToolError::authorization(
            "replica_read_only",
            "nothing is queued locally; re-run against the center",
        ));
    }
    let outcome = rc_memory::reuse::ReuseOutcome::from_slug(outcome).ok_or_else(|| {
        ToolError::validation(
            "bad_outcome",
            "outcome must be exactly one of worked|failed|partial|mismatch|stale",
        )
    })?;
    let key = MemKey::parse(handle).ok_or_else(|| {
        ToolError::validation(
            "bad_handle",
            "expected 'card:<64 hex>'; a malformed handle is never guessed into a row",
        )
    })?;
    // 复用只对**卡**有意义：锚没有被"用上"这一说，给锚句柄就是误用（不回显它）。
    if key.table != MemTable::Card {
        return Err(ToolError::validation(
            "bad_handle",
            "reuse takes a card handle ('card:<64 hex>'); anchors are not consumed",
        ));
    }
    // 读闸只走 `rc_memory::gate` 的单一实现（S1b-C4b1）：被拒与"行不存在"同形。
    if rc_memory::gate::lookup_visible(m, viewer, now_ms, &key)
        .map_err(ToolError::from)?
        .is_none()
    {
        return Err(ToolError::empty(
            "row_absent",
            "the handle does not resolve; nothing was written",
        ));
    }
    let event_id = rc_memory::reuse::record_reuse(m, &key.id, outcome.slug(), note, now_ms, by_agent)
        .map_err(|e| match e {
            // 防御支：上面已经过了同一道闭集。两处漂了就在这里红（码与上面同词）。
            rc_memory::reuse::ReuseError::UnknownOutcome => ToolError::validation(
                "bad_outcome",
                "outcome must be exactly one of worked|failed|partial|mismatch|stale",
            ),
            // 账层错误**委托** `From<MemError>` 的分型表，不写第二份真值表。
            rc_memory::reuse::ReuseError::Mem(e) => ToolError::from(e),
        })?;
    // 回执带卡与 outcome，**不带正文**（与 vote 的回执同一条守卫 12）。
    Ok(crate::ToolJson::from_value(json!({
        "schema_version": crate::SCHEMA_VERSION,
        "card": key.key_string(),
        "outcome": outcome.slug(),
        "event_id": event_id,
        "at_ms": now_ms,
    })))
}

/// 记忆库侧的计数读数。**零正文**（守卫 12 的诊断面那一半）。
/// 任务 10 给它加了**每轮抽取态**的反查（`opts.turn` + 读缝）：顶层既有格不动，后续增量
/// 只在其上增键（如 `freshness`）、不删不改；`turn` 给了才多挂 `"extraction"` 那一格——
/// 没问就那一格**不出现**（不是 null 占位）。
pub fn status(
    m: &MemoryHandle,
    read: Option<&dyn CycleRead>,
    opts: &StatusOptions,
    now_ms: i64,
) -> Result<crate::ToolJson, ToolError> {
    // 两个可选输入**互斥**：`turn` 要的是"这一轮的抽取态"，`entity` 要的是"从目录里筛名字"。
    // 同时给时两半读数会挂在同一个响应上（`extraction` + `entities`），调用方说不出
    // `entities_total` 是谁要的数。合并成一格也不行：那是一格两义（P-45 那一族）。
    if opts.turn.is_some() && opts.entity.is_some() {
        return Err(ToolError::validation(
            "turn_and_entity",
            "recollect_status takes either a turn handle or an entity name, not both",
        ));
    }
    // 空实体名是**输入校验**而不是"查无此人"：后者是合法读数（空数组），前者是调用方的 bug。
    // 判据走归一后的长度——与 `RememberRequest::new` 同一把尺子（守卫 V1：归一只有一家）。
    if opts.entity.is_some_and(|raw| rc_contract::normalize_body(raw).is_empty()) {
        return Err(ToolError::validation(
            "empty_entity",
            "the entity filter needs a non-empty name; blanks are never guessed into a lookup",
        ));
    }
    // G19 刀 1：counts 面没有实体目录，`entity` 过滤词在那里无处可挂。静默忽略过滤
    // 会把"我筛过了"读成"全量计数"，所以响亮拒绝——与上面 `turn_and_entity` 同一条
    // "互斥就报错"的纪律，而 `turn` 与 counts 不冲突（extraction 是每轮读数，不是目录）。
    if opts.view == StatusView::Counts && opts.entity.is_some() {
        return Err(ToolError::validation(
            "counts_and_entity",
            "view=counts returns counters only; drop view=counts to use the entity filter",
        ));
    }
    // 全量读一次再数。代价与 `export` 同阶（都是 O(库大小) 的只读扫描）。
    // 阶段 5 若它慢，改的是换成计数 SQL，**不是砍掉这个读数**——砍掉的代价见 GOALS §3 那条失效形状。
    let facts = reader::facts(m).map_err(ToolError::from)?;
    let n = |table: MemTable| facts.iter().filter(|f| f.key.table == table).count();
    // P5 的三格。**缝在场才有读数**，缺席就三格 `null`（P-54）。S3a 起 `quota_exhausted`
    // 是 `CycleHealth.quota_stopped` 的**原样搬运**（三态：`null` / `false` / `true`），
    // 不再做 `used >= limit` 那一步——生命周期累计（88）与"最近一次运行触没触顶"是两件事，
    // 旧口径把前者当后者，于是这一格恒 true。判定本身在 hub 的五个收尾口（naming / fact /
    // event / state / cycle，各写 0/1；其中 `cycle` 不是 pass、是周期收尾），
    // 归档在 `rc_ledger::counters::quota_stopped`；`status` 与 `doctor` 读的是同一份运行记录。
    //
    // cycle_health 只在"无 turn、无 entity"的纯 status 路径上调：opts.turn=Some 时调用方关心的
    // 是这一轮的抽取态（缝的另一方法 `turn_extraction`），P5 那一格里若缝失败就成了多余的失败。
    // 这是 controller 侧偏离 brief 字面 `match read` 的写法——那一条会把 `a_ghost_handle_on_status_is_empty`
    // 与 `status_带_turn_时把三格挂在_extraction_下面` 两条 P2 用例踩红：它们用
    // `Tap::default()` / `Tap::with_extraction(...)` 走 `status(turn=…, Some(&tap))`，这两条用例
    // **不打算测 P5**，所以 `health: None`，若 `cycle_health` 此刻被调就回 `store_unreachable`
    // 而非预期的 `turn_absent` / `extraction` 那一格的成功。报告里登记为 D-1。
    // P5 的三格 + P6 的两格 + S3a 的成本读数同一个 `if`、同一次 `cycle_health()`。
    // **不许**把 P6 拆成另一个 `match read`：`cycle_health()` 只能存在于"无 turn、无 entity"的
    // 纯 status 路径上——`a_ghost_handle_on_status_is_empty` 与 `status_带_turn_时把三格挂在
    // _extraction_下面` 两条存量用例用的是 `health: None` 的 `Tap`，多调一次就把它们从
    // "预期的成功/turn_absent"变成 `store_unreachable`（tools.rs 里 D-1 那段注释记过一次同样的踩法）。
    // 这几格在 turn/entity 路径上回 `null`：**键恒在场、值可为 null**（P-54 兑现的是前者），
    // 与 P5 三格同律；裁定 20 的"始终在场"说的是键，不是"每条路径都要有非空读数"。
    // 元组的第七个元素（`last_anomaly_scan_at_ms`）与 `last_derive_at_ms` **同族**：输入在账上、
    // 只能经缝搬进来（`water::last_extract_at_ms` 要 `LedgerHandle`，守卫 6 不许 rc-agent 拿它），
    // 所以它是 `Option`、缝缺席即 `null`。它**不**与 `attention_open` 同族（下面那一格是直读库、
    // 恒为数）——两格含义正好相反：一个是"多久没扫了"，一个是"现在挂着几条"。
    let (lag, last, quota_stopped, llm_calls_total, derive_last, derive_lag, last_anomaly_scan_at_ms) =
        if opts.turn.is_some() || opts.entity.is_some() {
            (None, None, None, None, None, None, None)
        } else {
            match read {
                Some(r) => {
                    let h = r.cycle_health()?;
                    // 余量在这里现算，不在 `mcp` 算：输入（游标）来自账、被数的行来自库，
                    // 而 `status` 是同时握着这两样的唯一一点（它本来就收 `&MemoryHandle`）。
                    // 缝在场时 `h.derive_cursor == None` 是"水位行还不存在"⇒ `from=None`
                    // ⇒ 数全库活行 = 真实的"一行都还没推导过"，所以这一格永远有生产者、
                    // 不是恒零占位（P-44）；缝缺席才是 `None`——两种"没有"不许混成一格。
                    let lag_rows = rc_memory::reader::count_rows_since(m, h.derive_cursor)
                        .map_err(ToolError::from)?;
                    (Some(h.lag_turns), h.last_success_at_ms,
                     h.quota_stopped, h.llm_calls_total,
                     h.last_derive_at_ms, Some(lag_rows), h.last_anomaly_scan_at_ms)
                }
                None => (None, None, None, None, None, None, None),
            }
        };
    // A1 的两格——**与 P5 三格不同**：恒出现，turn/entity 路径上也在。缝缺席 ⇒ null（P-54）。
    // **刻意不放在上面那个 `if` 里**：那里是 `cycle_health()` 的家，而 `cycle_health()` 只在纯
    // status 路径上调；新读数挂进去就跟着只在那条路径出场，与"恒出现"正好相反（硬约束 A1）。
    let (halted_scopes, lag_scopes) = match read {
        Some(r) => (Some(r.halted_scopes()?), Some(r.lag_scopes()?)),
        None => (None, None),
    };
    // 未列出数是父格的**派生读数**，所以它跟父格同生共死（S3b）：`lag_scopes` 缺席 ⇒    // `lag_scopes_unlisted` 也缺席（`null`），不许折成 0。`0` 会被读成"全部列出、一条没剩"，
    // 而这一面其实什么都没读到——P-54 点名的假零正是这个形状。
    let (lag_listed, lag_unlisted) = match &lag_scopes {
        Some(v) => {
            let take = v.len().min(MAX_STATUS_ENTITIES);
            (Some(take), Some(v.len().saturating_sub(take)))
        }
        None => (None, None),
    };
    // `Option` 而不是 `Vec` + `unwrap_or_default()`：缝缺席时那个 `vec![]` 会被 serde 渲染成
    // 空数组，于是 `status` 在节点面上把"这一面答不了"读成"没有 scope 落后"——正是 P-54
    // 与 P-44 各自点名的那一种最像成功的失败。`lag_listed` 就是上面那个 `take`，
    // 所以截断口径只有一个（不许这里再写第二个 `min`；`map` 里只在 Some 一支取值，
    // `unwrap_or(0)` 只是给它一个类型上的整数，永远不会被 None 那一支用到）。
    // P0-1 的两格。左边读转录库（只能经缝）、右边直读记忆库（本函数已经握着 `m`），
    // 两格都**恒在场**：缝缺席时左边是 `null`（P-54），右边恒为数（`0` 是"干净"这个读数，
    // 不是"读不到"——读失败经 `?` 让整条 `status` 报错，与 `attention_open` 同族）。
    let principal_claims = match read {
        Some(r) => r.principal_claims()?,
        None => None,
    };
    let agent_speaker_human_evidence =
        rc_memory::reader::facts_with_agent_speaker_but_human_evidence(m)
            .map_err(ToolError::from)?;
    let lag_objects: Option<Vec<serde_json::Value>> = lag_scopes.as_ref()
        .map(|v| v.iter().take(lag_listed.unwrap_or(0))
            .map(|(scope, lag)| json!({"scope": scope, "lag": lag}))
            .collect());
    // S3c：`freshness` 三格。三格的输入**全在手上**——`facts` 已经为上面的计数全量读过一次，
    // `lag`/`last` 就是 P5 那两格的同一个值——所以这一格不新增任何一次查询（零 DDL、零新读）。
    // 单位是秒而不是毫秒：这一格回答的是"多久没动了"，人读的是"两小时/三天"，而毫秒精度
    // 只会诱使消费面去比阈值（本任务明令不许有 tier/阈值，判定留给读的人）。
    // `null` 专表**从来没有**：空库没有行 ⇒ 没有"最新一行的时刻"；账上没有终态 pass 行
    // ⇒ 缝递 `None` ⇒ 没有"最后一次抽取"。两件事都不是 0 秒（P-54/P-44 那一族）。`as_of = 0`
    // 不是时刻：anchor 行这一列是读侧用字面量 `"0"` 补出来的（`reader.rs` 的 `select_sql`），
    // 所以 `> 0` 首先是在排掉**整张锚表**（否则每条锚行都以 1970 年参与取最大值）；
    // 而 `card.as_of_ms` 虽 NOT NULL 却没有任何 `CHECK(> 0)`、也没有写者会故意写 0——
    // "0 = 这一列没写"是安全但**未被强制**的约定。有效取值面因此只有 `card`。
    let newest_as_of_ms = facts.iter().map(|f| f.as_of_ms).filter(|ms| *ms > 0).max();
    // 饱和到 0：跨机写入/时钟回拨会让时刻落在 `now_ms` 之后，而"负的年龄"没有读法。
    let age_s = |ms: i64| now_ms.saturating_sub(ms).max(0) / 1000;
    let freshness = json!({
        "last_extract_age_s": last.map(age_s),
        "newest_memory_age_s": newest_as_of_ms.map(age_s),
        // 与 `distill_lag_turns` **同源同值**：它答的是"还有几轮没抽"，不是另一本账的差。
        "pending_turns": lag,
    });
    let index = match rc_index::index_status(m) {
        Ok(Some(r)) => json!({
            "built": true,
            "rows_indexed": r.rows_indexed,
            "norm_version": r.norm_version,
            "schema_version": r.schema_version,
            "rebuilt_at_ms": r.rebuilt_at_ms,
        }),
        // `NotBuilt` 与建索引失败必须分开：前者是**读数**（`built:false`），后者才走错误分型族。
        // 合并它的症状是"没建索引"被报成"库坏了"，而运维的下一步动作完全不同。
        Ok(None) => json!({ "built": false }),
        Err(e) => return Err(ToolError::from(e)),
    };
    let mut out = json!({
        "schema_version": crate::SCHEMA_VERSION,
        "source": source_of(m),
        "rows": {
            // canonical 只有两张表：旧 `event` / `assertion` 消费行现在都落 `card`
            // （见 `rc_memory::writers_x::put_event` / `put_assertion`），所以这里按表数一次就够，
            // 不按旧 kind 再数一遍（那会与 `card` 重叠，同一行被数两次）。
            "anchor": n(MemTable::Anchor),
            "card": n(MemTable::Card),
        },
        "open_rejections": facts.iter().filter(|f| f.rejected).count(),
        "open_retirements": facts.iter().filter(|f| f.retired).count(),
        // `is_some_and` 而不是 `map_or(false, ..)`：同一个谓词的两种写法，rustc/clippy 只认前者的形状。
        "expired": facts.iter().filter(|f| f.valid_to_ms.is_some_and(|t| t <= now_ms)).count(),
        // A1 的两格 + 未列出数：键恒在场（含 turn / entity 两条路径），值在缝缺席时是 `null`。
        // `lag_scopes_unlisted` 是 `lag_scopes` 的派生读数，所以父格缺席时它一起 `null`（S3b/P-54）。
        // 放在 P5 三格旁边是因为语义同族（都是运维可见性的裸读数），而判定一件也不在这里做：
        // 降序由 `water::lag_by_scope` 给，截断上限是 `MAX_STATUS_ENTITIES`，
        // "有没有落后"也不由本面重新比一遍（`lag_by_scope` 已经滤掉 lag ≤ 0）。
        "halted_scopes": halted_scopes,
        "lag_scopes": lag_objects,
        "lag_scopes_unlisted": lag_unlisted,
        "cooling_down": facts.iter().filter(|f| f.cooldown_until_ms.is_some_and(|t| t > now_ms)).count(),
        "dangling_attestations": reader::dangling_attestations(m).map_err(ToolError::from)?,
        // 与上面那一格同族：`status` 自己数库、恒出现、**没有 `null` 档**。`rc-agent` 的
        // `[dependencies]` 里本来就有 `rc-memory`，所以这一格不经过缝——缝是给"拿不到那个句柄的
        // 读数"准备的搬运道，告警数不在其列。读失败经 `?` 让整条 `status` 报错（P-61 的镜像面：
        // `status` 没有兜底哨兵），不折成 `0`：`0` 是"库自洽"这个**读数**。
        // 下面那一格（心跳）与它**正好相反**：走缝、可为 `null`，两格并排读才分得清
        // "挂着 0 条 + 从没扫过"与"挂着 0 条 + 刚扫过"。
        "attention_open": rc_memory::anomaly::open_count(m).map_err(ToolError::from)?,
        // 复核计数与 `attention_open` 同族：直读记忆库、恒出现、没有 null 档
        // （`rc-agent` 已依赖 `rc-memory`，这一格不经过缝）。两格相减得到的是"挂着且已经被
        // 复核过"的**累计**条数，不是"这一轮动过几条"——后者没有家（一轮盖几条只由
        // `recheck_attested` 的返回值知道，hub 那一侧把它丢掉、不进输出面）。所以这一格不许写成
        // `Option`——那会让消费面对同族的两个形状读法不同。
        // 读失败经 `?` 让整条 `status` 报错（P-61：`status` 没有兜底哨兵），不折成 `0`。
        "recheck_pending": rc_memory::recheck::recheck_pending(m).map_err(ToolError::from)?,
        "last_anomaly_scan_at_ms": last_anomaly_scan_at_ms,
        "index": index,
        // P5 的三格（§6.5）。三个 `Option` 的 `None` 一支靠 serde 默认渲染成 JSON `null`，
        // 所以"两面同形状"这一句在这里由**类型**兑现，而不是由额外分支兑现：
        // 缝缺席 ⇒ 三个 `None` ⇒ 三个键都在、值都是 `null`，`json!` 里一个特殊分支都不必写。
        "distill_lag_turns": lag,
        "last_extract_at_ms": last,
        // S3c：三格"多久没动了"，顶层一格（**不塞进 probe / extraction 那两块**：它不是某条
        // 路径的附注，而是每次 status 都该有的运维读数；`Option` 的 `None` 同样由 serde 渲染
        // 成 `null`，所以键恒在场、值可为 null 这一条由类型兑现，见上面那段）。
        "freshness": freshness,
        // S3a：这一格搬的是"最近一次**记过账**的运行是否触顶"（三态）——集合 = naming /
        // fact / event / state / cycle；不是累计比较，也不是"全仓最后一次进程退出"。
        "quota_exhausted": quota_stopped,
        // R3：累计数另名，只作成本读数；它不参与上面那一格的判定。
        "llm_calls_total": llm_calls_total,
        // P6 的两格，同一条 `cycle_health()` 调用出来的：`last_derive_at_ms` 是缝搬的时间，
        // `derive_lag_rows` 是 status 用那条游标现算的余量（生产者在上面那个七元组那一支）。
        // 两个 `Option` 的 `None` 一支同样由 serde 渲染成 JSON `null`，所以键恒在场（P-54）。
        "last_derive_at_ms": derive_last,
        "derive_lag_rows": derive_lag,
        // 这一格是**能力声明**（`queryable` = 状态可反查），与下面 `extraction` 那格的
        // **每轮读数**同在一张 JSON 里且不许相等（P-26 的顶层读数）。
        // P0-1 的两格（口径见 `rc_memory::reader` 与 `rc_ledger::transcript` 各自的实现点）：
        // 前者是"有几个人盖过章"，后者是"用户说的被记成 agent 说的"还剩几行。只给数，不给正文。
        "principal_claims": principal_claims,
        "facts_with_agent_speaker_but_human_evidence": agent_speaker_human_evidence,
        "extract_state": crate::capabilities().extract_state,
        "capabilities": crate::capabilities(),
    });
    // G19 刀 1：counts 面把**工具能力声明**整格拿掉（`capabilities` 是工具名清单 +
    // 沉默格清单的家——DSH 实测 4.6KB 里最大的一块）。`extract_state` 留下：它是一个
    // 词的声明，不是清单。删键而不是"构造两份字面量"：两份字面量是下一次漂移的地方。
    if opts.view == StatusView::Counts {
        out.as_object_mut().expect("json! 的字面量是对象").remove("capabilities");
    }
    if let Some(raw) = opts.turn {
        if !is_turn_uid(raw) {
            return Err(ToolError::validation(
                "bad_turn_handle",
                "expected '<64 lowercase hex>' turn_uid; a malformed handle is never guessed into a row",
            ));
        }
        // 节点面（`hook`）拿不到水位库 ⇒ 缝缺席 ⇒ **响亮拒绝**而不是给 `null`：这一格是调用方
        // **主动要的**，`null` 会把"这一面答不了"读成"这一轮没有状态"（P-54 那一族的更严版）。
        let read = read.ok_or_else(|| ToolError::unavailable(
            "extraction_not_readable",
            "this face has no transcript access; ask the center",
        ))?;
        // **同一个 `None`、另一个分型**（P-62）：这里问的是"你给的句柄在不在"，不在就是查无。
        let fields = extraction_readout(read, raw)?
            .ok_or_else(|| ToolError::empty(
                "turn_absent",
                "the handle does not resolve to a transcript turn; it may have been purged",
            ))?;
        // 挂在 `"extraction"` 下面而不是摊平：顶层已有一格叫 `extract_state` 的**声明**，
        // 同名摊平会把声明与读数撞成一格——那正是 P-45 要防的事的反面（一格两义）。
        out.as_object_mut().expect("json! 的字面量是对象").insert(
            "extraction".to_string(),
            serde_json::Value::Object(fields),
        );
    }
    // 目录三格。`opts.turn.is_some()` 时**不长出**这三格：那一支是"问某一轮的抽取态"，
    // 顺手附一份全库实体目录就是一格两义（P-45 家族的反面：一个格同时是"这一轮的读数"
    // 和"全库的读数"），而 hook 的 `None` 位置因此也保持"这一面只有既有格、不额外长出目录"的形状。
    // G19 刀 1：`view=counts` 同样不长出（counts 面无实体名——`entity` 过滤在那一支已被拒，
    // 这里防的是"无过滤也带全量目录"的缺省形状漏进 counts 面）。
    if opts.turn.is_none() && opts.view == StatusView::Full {
        let dir = rc_memory::entity_directory(m).map_err(ToolError::from)?;
        // 比较的两侧各过一次归一（P-51）：库里存的仍是入参原样
        // （`writers_x` 那两支写入都是原样存标题与别名），而 `compute_mem_id` 是归一之后算的。
        // 于是 `"老王 "` 与 `"老王"` 命中同一个人，而 `"老 王"` 是另一个名字（不模糊匹配）。
        let want = opts.entity.map(rc_contract::normalize_body);
        let matched: Vec<&rc_memory::EntityEntry> = dir.iter()
            .filter(|e| match &want {
                None => true,
                Some(w) => rc_contract::normalize_body(&e.name) == *w
                    || e.aliases.iter().any(|a| rc_contract::normalize_body(a) == *w),
            })
            .collect();
        let listed = &matched[..matched.len().min(MAX_STATUS_ENTITIES)];
        let cells = listed.iter().map(|e| json!({
            "id": e.key, "kind": e.kind, "name": e.name,
            "aliases": e.aliases, "refs": e.refs, "latest_as_of_ms": e.latest_as_of_ms,
        })).collect::<Vec<_>>();
        // `entities_total` 是**命中集**的大小，不是全库实体的大小（本轮裁定）。
        // 理由：与 `entities` 同一批、与 `unlisted` 自洽（三者相加恒成立），而且它跟着
        // `rows.card` 那一族的口径走——阶段 1 的 `rows` 数的就是"这一类有几条"。
        // 代价说清楚：带 `entity` 过滤时，`entities_total` **不等于** `rows.anchor` 的实体侧口径，
        // 读的人必须知道 total 是过滤后的。这条写在错误消息之外的地方，是因为它是**读数语义**，
        // 而读数语义只有一个家：用例 `查无此人时目录是空数组而不是错误` 与
        // `目录截断到上限并且明写还有几个未列出` 各钉住一侧。
        let obj = out.as_object_mut().expect("json! 的字面量是对象");
        obj.insert("entities".to_string(), serde_json::Value::Array(cells));
        obj.insert("entities_total".to_string(), json!(matched.len()));
        // 第三个调用点（`lib.rs` 那段注释预告的就是这一刻）。**不许在这里自己减**（P-52）：
        // 走这个函数，`debug_assert` 才自动生效——三个数自相矛盾时要响亮地炸，
        // 而不是被饱和减法折成一个看起来正常的 0（G7）。
        obj.insert("entities_unlisted".to_string(),
                   json!(crate::unlisted_count(matched.len(), listed.len(), 0)));
    }
    Ok(crate::ToolJson::from_value(out))
}

// ── W38b 刀 D（G19）：导入→可查的两阶段自查面 ─────────────────────────────
//
// `recollect_ingest_status(session_key)`：导入侧在导入之后问"这些轮到了哪一步"的唯一入口
// （hermes G14 与 DSH 同一痛点：导入后全靠瞎等）。三个读数经读缝
// `CycleRead::ingest_counts` 搬进来——守卫 6 不许 rc-agent 碰 rc-ledger，与 `status`
// 的 P5 三格同一条缝纪律，所以实现方只有 mcp 一家。本函数只做两件事：
// 查无此会话的分型（`no_rows`——mcp 面按**码**把它翻成 isError=false，谁发都一样）
// 与回执的键名拼装。键名只有一个家，就是下面这个 json!。

/// 一个会话的两阶段读数。`turns_pending` 不进缝：它是**本函数的派生读数**
/// （总数 − 已到终态，**钳到零**——`purge` 的残留终态行会让减法短暂为负，那时的
/// 正确读数是"没有积压"，不是负数也不是报错。注意不能用 `saturating_sub`：
/// 它饱和在 `i64::MIN`，`1 - 3` 照样给 `-2`——钳到零必须显式 `.max(0)`）。
pub fn ingest_status(
    read: &dyn CycleRead,
    session_key: &str,
) -> Result<crate::ToolJson, ToolError> {
    let Some(c) = read.ingest_counts(session_key)? else {
        return Err(ToolError::empty(
            "no_rows",
            "no visible turns for this session key",
        ));
    };
    Ok(crate::ToolJson::from_value(json!({
        "session_key": session_key,
        "turns_total": c.turns_total,
        "turns_distilled": c.distilled,
        "turns_pending": (c.turns_total - c.distilled).max(0),
        "last_arrival_ms": c.last_arrival_ms,
    })))
}

// ── W38a 刀 B：转录检索面（细案 2026-11-08-w38a-transcript-search §三） ─────
//
// 三个工具函数是 `rc_search::transcript` 三个读数在工具层的唯一出口（mcp 不许出现
// `rc_search` 这个词——P-47 的同一条纪律），所以这里负责两件事：
// ① 把库层信封排成工具回执的 JSON（键名只有这一个家；命中轮带细案 §三 点名的五格 +
//    W39 §二.4 的归属两列——`device_id` 不进命中轮，回放/清单工具也只收 session_key，
//    回放信封才带 device 与归属）；
// ② 把库层错误折进 ToolError 分型族（NoQueryTokens=Validation，读不出来=Unavailable）。
// 正文格（snippet / body）原样透传——原文本来就在转录层，不新增暴露面。

/// `recollect_search` 工具层的返回：wire 载体（`envelope`）+ 审计两格读数。
/// `hits` / `truncated` 从信封里拆出来给门面（mcp 的 audit_log detail 要用），
/// mcp 因此不必解析自己刚生成的 JSON 文本、也不必另开一份真值。
pub struct SearchReadout {
    pub envelope: crate::ToolJson,
    pub hits: usize,
    pub truncated: bool,
}

/// 库层错误 → ToolError 的唯一映射（本 crate 内）。空读数**不是**错误的一支——
/// 空命中与"命中全关在闸外"都是 `Ok`，分型靠信封的 `matched`（细案 §五，D32 教训）。
fn transcript_error(e: rc_search::transcript::TranscriptSearchError) -> ToolError {
    use rc_search::transcript::TranscriptSearchError as E;
    match e {
        // 查询切不出任何词：调用方的输入问题（与 `empty_query` 同一型），不是读数故障。
        E::NoQueryTokens => {
            ToolError::validation("no_query_tokens", "the query yields no search terms")
        }
        // 两支"读不出来"对调用方是同一个下一步：跑 doctor。
        E::Sql(_) | E::LayerUnreadable => {
            ToolError::unavailable("store_unreachable", "run: doctor")
        }
    }
}

/// `recollect_search` 的工具层：独立于 recall，只搜转录层原话（不查 memory.db、
/// 不占 recall 的注入预算）。`limit` 的收口（1..=20、`0` 取缺省 10）住库层，
/// 工具层不抄第二把尺。
pub fn search(
    t: &TranscriptHandle,
    viewer: &rc_memory::gate::Viewer,
    query: &str,
    limit: usize,
) -> Result<SearchReadout, ToolError> {
    let out = rc_search::transcript::search_turns(
        t,
        rc_search::transcript::Trust::Ownership(viewer),
        query,
        limit,
    )
    .map_err(transcript_error)?;
    let hits = out
        .hits
        .iter()
        .map(|h| {
            json!({
                "turn_uid": h.turn_uid, "session_key": h.session_key, "role": h.role,
                "created_at_ms": h.created_at_ms, "snippet": h.snippet,
                "owner_agent_id": h.owner_agent_id,
                "owner_principal_id": h.owner_principal_id,
            })
        })
        .collect::<Vec<_>>();
    let envelope = crate::ToolJson::from_value(json!({
        "hits": hits,
        "matched": out.matched,
        "truncated": out.truncated,
        "coverage": {"indexed": out.coverage.indexed, "total": out.coverage.total},
    }));
    Ok(SearchReadout { hits: out.hits.len(), truncated: out.truncated, envelope })
}

/// `recollect_session` 的工具层：该会话的轮正文回放（同闸）。整个会话不可见 ⇒
/// `no_rows`（与"查无"同形、不泄露存在性，细案 §三；mcp 面 isError=false）。
/// W39 刀 E：信箱封随 [`rc_search::transcript::SessionReplay`] 补 `device_id` 与
/// 归属两列（细案 §二.4："知道跟谁说话"的落点）。
pub fn session(
    t: &TranscriptHandle,
    viewer: &rc_memory::gate::Viewer,
    session_key: &str,
    limit: usize,
    after_uid: Option<&str>,
) -> Result<crate::ToolJson, ToolError> {
    let replay = rc_search::transcript::session_turns(
        t,
        rc_search::transcript::Trust::Ownership(viewer),
        session_key,
        limit,
        after_uid,
    )
    .map_err(transcript_error)?;
    let Some(replay) = replay else {
        return Err(ToolError::empty(
            "no_rows",
            "no visible turns for this session key",
        ));
    };
    Ok(crate::ToolJson::from_value(json!({
        "session_key": replay.session_key,
        "device_id": replay.device_id,
        "owner_agent_id": replay.owner_agent_id,
        "owner_principal_id": replay.owner_principal_id,
        "turns": replay
            .turns
            .iter()
            .map(|r| json!({
                "turn_uid": r.turn_uid, "role": r.role, "body": r.body,
                "created_at_ms": r.created_at_ms,
            }))
            .collect::<Vec<_>>(),
    })))
}

/// `recollect_turn` 的工具层（W39 细案 §3.4）：凭 `turn_uid` 取该轮正文与同会话
/// 前后邻居。**先分形状、再问存在**（与 [`report`] 的 `turn_uid` 那一支同一道门）：
/// 形状不合法是调用方的 bug（`bad_turn_handle`），库层 `Ok(None)`（轮不存在与
/// 被闸挡同形）折成 `no_rows`——isError=false、不泄露存在性（细案 §3.4）。
/// `before`/`after` 的收口（≤10）住库层 `MAX_WINDOW_SIDE`，这里不抄第二把尺。
pub fn turn(
    t: &TranscriptHandle,
    viewer: &rc_memory::gate::Viewer,
    turn_uid: &str,
    before: usize,
    after: usize,
) -> Result<crate::ToolJson, ToolError> {
    if !is_turn_uid(turn_uid) {
        return Err(ToolError::validation(
            "bad_turn_handle",
            "expected '<64 lowercase hex>' turn_uid; a malformed handle is never guessed into a row",
        ));
    }
    let window = rc_search::transcript::turn_window(
        t,
        rc_search::transcript::Trust::Ownership(viewer),
        turn_uid,
        before,
        after,
    )
    .map_err(transcript_error)?
    .ok_or_else(|| {
            // 与 `session` 的 `no_rows` 同一分型：一个轮读不到，可能是没有、也可能是不给看。
            ToolError::empty("no_rows", "no visible turns around this turn_uid")
        })?;
    // 信封键名只有这一个家；正文格原样透传（原文本来就在转录层，不新增暴露面）。
    Ok(crate::ToolJson::from_value(json!({
        "turn_uid": turn_uid,
        "session_key": window.session_key,
        "device_id": window.device_id,
        "owner_agent_id": window.owner_agent_id,
        "owner_principal_id": window.owner_principal_id,
        "turns": window
            .turns
            .iter()
            .map(|r| json!({
                "turn_uid": r.turn_uid, "role": r.role, "body": r.body,
                "created_at_ms": r.created_at_ms,
            }))
            .collect::<Vec<_>>(),
        "truncated_before": window.truncated_before,
        "truncated_after": window.truncated_after,
    })))
}

/// `recollect_sessions` 的工具层：可见会话清单（细案 §三），回放（[`session`]）的
/// 伴生读数。库层给全量（转录面会话数是个位到十位量级），工具层收可选 `limit`
/// 并如实报截断（G7）。
pub fn sessions(
    t: &TranscriptHandle,
    viewer: &rc_memory::gate::Viewer,
    limit: Option<usize>,
) -> Result<crate::ToolJson, ToolError> {
    let all = rc_search::transcript::visible_sessions(
        t,
        rc_search::transcript::Trust::Ownership(viewer),
    )
    .map_err(transcript_error)?;
    let truncated = limit.is_some_and(|n| all.len() > n);
    let listed: Vec<_> = match limit {
        Some(n) => all.iter().take(n).collect(),
        None => all.iter().collect(),
    };
    Ok(crate::ToolJson::from_value(json!({
        "sessions": listed
            .iter()
            .map(|s| json!({
                "session_key": s.session_key, "source_agent": s.source_agent,
                "turns": s.turns, "first_ms": s.first_ms, "last_ms": s.last_ms,
                "project_hint": s.project_hint,
            }))
            .collect::<Vec<_>>(),
        "truncated": truncated,
    })))
}
