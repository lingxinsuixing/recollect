//! `rc-agent`：全仓**唯一被允许产出正文**的 crate（守卫 12 的执法点）。
//!
//! 前面六个任务里"能读到正文"的东西越来越多（`RowFacts`、`Scored`、`Decision`），
//! 而系统里唯一合法的出口只有两处：agent 工具面（C2）与人类的 `export`（C3）。
//! 本 crate 把"正文只能变成四种载体之一"立成类型层约束：`InjectionBlock` / `ToolJson` /
//! `ExportDoc` / `Narrative`。反方向的红线同在此处：运维/诊断/错误面**零正文、零路径、零密钥**——
//! `ToolError` 携带的是类别码与 next_action **常量串**，不是拼接出来的用户内容。
//!
//! 四个载体的字段都是私有的、构造器都是 `pub(crate)`：`hook`（任务 9）与 `mcp`（任务 10）
//! 能拿到它们、能 `text()` 它们，但既不能把内部拆成自由字符串往外转发，也不能凭空造一个
//! "已经装好正文"的出口对象。那两件事各有一条编译失败用例守着
//! （`tests/compile_fail/body_needs_a_carrier.rs` 与 `export_needs_a_key.rs`）。
//!
//! 唯一的两处例外是 `ToolJson::passthrough` 与 `ToolJson::with_transcript_source`
//! （W39 刀 D）：远端转录代理发生在 `mcp` 里，而"中心信封原样透传"与"本地回落补出处"
//! 都必须经本类型的门——它们不拆内部、不拼正文，只是给一个已经排好的 JSON 盖上出处章。

pub mod error;
pub mod export;
pub mod l0;
pub mod recall;
pub mod sinks;
pub mod surface;
pub mod tools;
pub mod views;
// 注意：**本 crate 没有 `render` 模块**——围栏前缀（V3）的唯一家是 `surface.rs` 的 `probe_block`，
// 第七个模块不许存在（任务 11 用一句 grep 把本 crate 源码里的模块声明清单执法为零余量）。

pub use error::{AgentErrorKind, ToolError};
pub use export::{render_export, ExportRequest};
// Phase 4（I-5a）：`recall` / `recall_narrative` 的**签名一字不动**地留在导出面上
// （mcp 与 hook probe 两个调用点），新增的是 `*_with` 那一对与 `RecallOpts`/`Intent`。
// `Default` 那一份读数与旧函数逐字节相同，所以 I-5b 接工具面时不必知道旧行为长什么样。
pub use recall::{
    recall, recall_narrative, recall_narrative_with, recall_with, validate_query, Intent,
    NarrativeView, RecallOpts, RecallRequest,
};
pub use sinks::{CycleHealth, CycleRead, IngestCounts, TurnExtraction, TurnSink};
pub use surface::{brief_block, probe_block, probe_via_search, status_block, FENCE};
pub use tools::{
    ingest_status, remember, report, search, session, sessions, source_of, status, vote,
    RememberRequest, SearchReadout, StatusOptions, StatusView, VoteStance, MAX_REMEMBER_CHARS,
};

/// C2 的演进规则：字段只加不减，未知字段忽略。它必须**在类型上就有一个位置**，
/// 否则第一个加字段的人会把版本号和 capabilities 各写一份。
///
/// M2.5 从 1 升到 2：`recollect_recall` 的**默认输出**从 JSON 行变成自然语言 brief。
/// 加一个可选字段不升版本；改默认行为要升——消费方否则会把"模型突然读到散文"当成解析失败。
pub const SCHEMA_VERSION: u32 = 2;

/// "还有 N 行未列出"的**唯一算法**（G7：沉默与截断都要能自证，但只许有一个真相）。
/// 三个入参各自是这一面"能看见的 / 已经列出的 / 已经作为沉默单独交代过的"。
/// 调用点共三处，各自传的是什么写在各调用点旁边：
/// `export`（本 crate）传 `(facts.len(), listed, 0)`——被抑制的行**也列在上面**，所以不扣；
/// `recall`（本 crate）查询臂传 `(total_after_filter, rows.len(), 0)`——第一个数是
/// **过滤后**在场的条数，不是 `rank` 全集（T10 起）：被过滤器挡掉的那一族不该进"还有 N 条我可以要却没给"，
/// 否则未列出数会把过滤说成截断。空过滤器下两者相等，所以这是口径变更而非行为变更。
/// `tools`（本 crate）的 entity 目录传 `(matched.len(), listed.len(), 0)`——第一个数是**命中集**
/// 的大小（P-52）：它与同一批 `entities`、`entities_unlisted` 三者相加恒成立。
/// 任务 8 的 `status` 是**全量计数**面（`reader::facts` 一次数到底，没有"未列出"这个概念），
/// 所以它不调这里，更不许自己减——将来 status 若长出截断，那一支也必须调**这个**函数。
/// `debug_assert` 是刻意的：三个数一旦自相矛盾（列出的比能看见的还多），这里必须**响亮**地炸，
/// 而不是靠饱和减法把矛盾咽下去（G7 禁止把异常折成看起来正常的读数）。
///
/// W36 小本本的 `unlisted` 读数住 `rc_core::ops::notebook_unlisted`——**同一口径的镜像**，
/// 不是本函数的调用点：依赖方向是 rc-agent → rc-store/rc-memory/rc-search/rc-index，
/// 而 rc-core 的依赖清单被 W36 细案 §6 钉死为 rc-contract/rc-secret/rusqlite/serde 系
/// （编译期碰不到 rc-memory/rc-index），所以 rc-core **不能**反过来依赖本 crate。
/// 两处各有一条互指注释（`ops.rs` 的 `notebook_unlisted` 档头），改算法必须两边同改；
/// 若日后算法下沉到 rc-contract，镜像处是第一个换回单一真相的调用点。
pub fn unlisted_count(candidates: usize, listed: usize, silent: usize) -> usize {
    debug_assert!(
        listed.saturating_add(silent) <= candidates,
        "unlisted accounting contradicts itself: candidates {candidates} listed {listed} silent {silent}"
    );
    candidates.saturating_sub(listed).saturating_sub(silent)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Capabilities {
    pub schema_version: u32,
    /// MCP 工具名的**唯一清单**（`mcp` 的 `tools/list` 与 `tool_defs` 都从这里取）。
    /// 前十二件是记忆面（沿用旧名），A9 加 `recollect_report`，波二刀 A 再加 `recollect_reuse`
    /// （"这张卡帮没帮上忙"的写入口），W38a 刀 B 再加转录检索面
    /// 三件（`recollect_search/session/sessions`，细案 `2026-11-08-w38a-transcript-search.md`
    /// §三：独立于 recall，只搜转录层原话），W38b 刀 D（G19）再加自查两件
    /// （`recollect_whoami` / `recollect_ingest_status`——身份四格与会话两阶段读数）；
    /// 后二十八件是核心陪伴层（工具契约 §2.2 的二十三件 + W36 小本本四件 + core.db
    /// 治理小刀③的 `core_relationship_update`，细案 `2026-11-07-w36-notebook-design.md` §3）。
    ///
    /// 形状是 **Vec** 而不是定长数组：serde 只给 ≤32 的数组实现 `Serialize`，
    /// 目录第 33 件（W36）起数组形状就编译不过了；清单仍然只有这一处字面来源。
    pub tools: Vec<&'static str>,
    pub forms: [&'static str; 3],
    pub silence_cells: Vec<String>,
    pub max_form_lines: usize,
    pub norm_version: u32,
    /// 抽取侧的**能力**声明（任务 10 改判，坑 P-26）：阶段 1 这里报的是“还没抽”那个状态串，
    /// 那是把"当前状态"当"系统能力"报——而抽取上线之后它永远为假，且它同时是 `remember`
    /// 那一格的**取数来源**（P-45 的同源自证）。现在它是 `queryable`，意思是
    /// "每 turn 的抽取态**可反查**"；每轮的**读数**住 `extraction` 那一格与 `remember` 的顶层。
    pub extract_state: &'static str,
    /// `remember` 的单次正文上限，**单位字符**。它进这张表是因为 agent 只能从声明面读上限——
    /// 错误 hint 里写死数字会有两处真相（`tools_shape.rs` 有一条用例专门断 hint 不含数字）。
    pub max_remember_chars: usize,
}

pub fn capabilities() -> Capabilities {
    Capabilities {
        schema_version: SCHEMA_VERSION,
        // 记忆面前六件：五个旧名与架构 §5 那张表逐字相同（recall/remember/vote/status/export）；
        // A9 加 `recollect_report`（反馈闭环的最小入口），其后依次追加 `recollect_reuse`（波二刀 A）
        // 与 W38a/W38b/W39 刀 B 的六件（转录检索四件 + 自查两件），合计十三件。
        // `probe` **不是工具**，它是 `hook` 的 stdout 形状（见 §2 的 V12）。
        // 核心层二十八件：二十三个名字逐字取自工具契约 §2.2（`core_brief` 到
        // `core_handoff_close`），加 W36 小本本四件（`core_notebook_append/list/search/expand`，
        // 名字取自细案 `2026-11-07-w36-notebook-design.md` §3），再加 core.db 治理小刀③
        // 的 `core_relationship_update`（payload 的 relationship 段此前唯一零写者；
        // 写口形状照 affect——agent 主动写，不接蒸馏/程序派生生产者）；
        // 它们与记忆工具同住这一张清单，所以 `tools/list`、`tool_defs` 的对撞与
        // `usage_counter` 的成员资格判据（R106）都不必为第二个来源开一处例外。
        tools: vec![
            "recollect_recall",
            "recollect_remember",
            "recollect_vote",
            "recollect_status",
            "recollect_export",
            "recollect_report",
            // 波二刀 A（G1 复用反馈）：`recollect_reuse`——"这张卡帮没帮上忙"的写入口。
            // 与 `recollect_vote` 分轴（表态=真假，复用=有没有用），不进 vote 的闭集。
            "recollect_reuse",
            // W38a 刀 B：转录检索面三件（名字取自细案 2026-11-08 §三）。
            "recollect_search",
            "recollect_session",
            // W39 刀 B（细案 §3.4）：按轮取原文——凭 turn_uid 取该轮与同会话前后邻居。
            "recollect_turn",
            "recollect_sessions",
            // W38b 刀 D（G19）：自查两件——身份四格与会话两阶段读数。
            "recollect_whoami",
            "recollect_ingest_status",
            "core_brief",
            "core_chinese_expression_get",
            "core_chinese_expression_set",
            "core_persona_get",
            "core_persona_propose",
            "core_persona_apply",
            "core_persona_revert",
            "core_persona_reject",
            "core_subject_get",
            "core_subject_update",
            "core_interaction_get",
            "core_interaction_touch",
            "core_affect_get",
            "core_affect_update",
            // 治理小刀③：relationship 写口（payload 11 段里唯一零写者的那一段）。
            "core_relationship_update",
            "core_loop_list",
            "core_loop_upsert",
            "core_loop_close",
            "core_episode_pin",
            "core_episode_unpin",
            "core_episode_expand",
            "core_handoff_write",
            "core_handoff_list",
            "core_handoff_close",
            "core_notebook_append",
            "core_notebook_list",
            "core_notebook_search",
            "core_notebook_expand",
        ],
        forms: ["silent", "signpost", "full"],        // 从 GateCell 现算，不写字面量：这张清单是 agent 面"沉默有几种原因"的唯一声明处，
        // 写死它就等于给"格子改名"留了一条不会红的路（`errors_are_four.rs` 最后一条用例逐字对撞它）。
        silence_cells: rc_search::gate::GateCell::ALL.iter().map(|c| c.slug()).collect(),
        max_form_lines: rc_search::gate::MAX_FORM_LINES,
        norm_version: rc_store::capability::NORM_VERSION,
        extract_state: "queryable",
        max_remember_chars: tools::MAX_REMEMBER_CHARS,
    }
}

/// **正文的三种合法出口**。字段私有 + 构造器 `pub(crate)`：
/// `hook` 与 `mcp` 能拿到它们、能 `text()` 它们，但不能凭空造一个，也不能把内部拆出来自由拼接。
/// 这一步把守卫 12 从"评审时记得别打印"变成"编译不过"。
#[derive(Debug)]
pub struct InjectionBlock { inner: String }
#[derive(Debug)]
pub struct ToolJson { inner: serde_json::Value }
#[derive(Debug)]
pub struct ExportDoc { inner: String }
/// LLM 面自然语言正文（`brief`/`card`/`expand`）。与 `ExportDoc` 一样是不可自由拼接的文本，
/// 区别在读者：`ExportDoc` 给人，`Narrative` 给模型；两者都只能由本 crate 的渲染器构造。
#[derive(Debug)]
pub struct Narrative { inner: String }

impl InjectionBlock {
    /// **控制字符在这里变成空格**：物理行数是 C2 承诺的一部分，而承诺必须由代码兑现，
    /// 不能由"夹具里正好没有换行"兑现。替换而不是删除——删除是静默截断。
    /// 放在构造函数里而不是渲染函数里：这条性质属于载体，不属于某一次渲染。
    fn one_line(raw: &str) -> String {
        raw.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
    }

    pub(crate) fn from_lines(lines: Vec<String>) -> Self {
        // 硬截断在**这里**（不是在调用方）：上限与它的执行必须在同一个类型上，
        // 否则第一个"我想多给一行"的改动就会把它绕开，而 C2 的承诺是"硬"上限。
        // `take` 数的是**物理行**，所以每一行必须先过 `one_line`：
        // 一条正文里带 `\n` 的记忆（`normalize_body` 只折行首尾、不删正文内部的换行）
        // 会让"串数"与"stdout 的物理行数"脱钩，第 4 行起就没有围栏前缀（V3 被破）。
        InjectionBlock {
            inner: Self::flatten(lines, true),
        }
    }

    /// brief 载体（W34 续接包）的构造底座：与 [`Self::from_lines`] 同一条 `one_line` 纪律
    /// （控制字符变空格、逐物理行过），但**不受 probe 面 `MAX_FORM_LINES` 的 3 行额度管辖**。
    /// 那个 3 是探针面的行预算（gate `fit` 的同一把尺）；brief 的行预算住 rc-core 的分区上限
    /// （`CAP_HANDOFF_PREV`），载体不设第二把尺。围栏前缀仍由调用方逐行落——
    /// "每行前缀"（V3）在 brief 载体上与 probe 载体同一条。
    pub(crate) fn from_brief_lines(lines: Vec<String>) -> Self {
        InjectionBlock { inner: Self::flatten(lines, false) }
    }

    /// `one_line` + 可选的 probe 面 3 行硬截断。两支唯一的差别就是那一格 `take`。
    fn flatten(lines: Vec<String>, probe_cap: bool) -> String {
        let mut flat: Vec<String> = lines.into_iter().map(|l| Self::one_line(&l)).collect();
        if probe_cap {
            flat.truncate(rc_search::gate::MAX_FORM_LINES);
        }
        flat.join("\n")
    }
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    /// 沉默的表示就是它（V3）：空串 ⇒ `stdout.is_empty()`。
    pub fn text(&self) -> String {
        self.inner.clone()
    }
}
impl ToolJson {
    pub(crate) fn from_value(v: serde_json::Value) -> Self {
        ToolJson { inner: v }
    }
    /// W39 刀 D：把**中心网络信封原样**装进工具回执（`/v1/transcript/*` 的 200 JSON）。
    /// 只有一个读者——`mcp` 的远端转录代理臂：那份信封由中心按同一套 DTO 序列化，
    /// 已带 `epoch` 与 `source:"center"`，边缘再拆一遍就是第二份形状。
    /// 它是本类型仅有的两个 `pub` 构造口之一（另一个是 `with_transcript_source`），
    /// 因为"远端代理"这件事发生在 rc-agent 之外；正文出口仍然只有本 crate 能造。
    pub fn passthrough(v: serde_json::Value) -> ToolJson {
        ToolJson { inner: v }
    }
    /// W39 刀 D：给**本地回落**的信封补出处三格（细案 §3.2/§五.6）：
    /// `source:"local"`、`degraded` 如实，`remote_error` 有故障码才插。
    /// `inner` 不是对象时**原样返回**（不 panic、不改形状）：信封形状由各读数自己负责，
    /// 这一层只做标注，不做第二次序列化。
    pub fn with_transcript_source(
        mut self,
        source: &str,
        degraded: bool,
        remote_error: Option<&str>,
    ) -> ToolJson {
        if let Some(obj) = self.inner.as_object_mut() {
            obj.insert("source".to_string(), serde_json::Value::String(source.to_string()));
            obj.insert("degraded".to_string(), serde_json::Value::Bool(degraded));
            if let Some(err) = remote_error {
                obj.insert("remote_error".to_string(), serde_json::Value::String(err.to_string()));
            }
        }
        self
    }
    pub fn text(&self) -> String {
        self.inner.to_string()
    }
}
impl ExportDoc {
    pub(crate) fn from_markdown(s: String) -> Self {
        ExportDoc { inner: s }
    }
    pub fn text(&self) -> String {
        self.inner.clone()
    }
}
impl Narrative {
    pub(crate) fn from_text(s: String) -> Self {
        Narrative { inner: s }
    }
    pub fn text(&self) -> String {
        self.inner.clone()
    }
}
