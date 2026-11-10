//! S2b-1 / S2c-1：fact 与 event 两个 pass 的 prompt 面（设计 `2026-10-26-s2-prompt-v0`
//! §2.2、§2.3、§4）。
//!
//! 两个 pass **不合并**：各自一次调用、各自的 canary、各自的 `pass` 值、各自的审核问题。
//! 文件里各份模板（当前 12 份，名单见各 `*_system`）共用同一套 `assemble`。
//!
//! fact 一次调用只问一个 family（`decision|promise|plan`），event 一次调用只问
//! "已经发生了什么"。两者都只见两类短引用：目标轮 `t1..tN`（= `batch.rows` 顺序）与
//! 候选锚 `a1..aN`（= `candidates` 顺序）。`anchor_id`、`turn_uid`、毫秒、owner 一律
//! 不进 prompt——它们由程序在写库时补。
//!
//! 三条纪律：
//! 1. **canary 只在这里**：本文件每份模板各有一个 canary 常量（当前 12 个，与各
//!    `*_system` 一一对应），且各自只出现在自己 system 的首行一次（`assemble` 保证）。`prompt.rs`
//!    的四份旧模板一字不动，新 pass 不借它们的 canary。
//! 2. **模板复用 `prompt::assemble`**：方法/标准/正例/反例/尾段的拼装顺序与 `prompt.rs` 的四份作业同一处，
//!    否则"形状只有一份"就成了各写各的。
//! 3. **渲染前必过 `prepare`**：与 `extract_user` / `a1_user` 同一条剪枝纪律；时间用固定
//!    `+08:00` 的分钟级 ISO（`format_local_ms` 的等价形态），不渲染毫秒。
//!
//! 设计 §7 曾写"prompt 常量仍住 `prompt.rs`（canary 唯一家）"。本片按派发档
//! `2026-10-26-s2b1/ORDER.md` 与 `2026-10-26-s2c1/ORDER.md` 执行：允许清单不含
//! `prompt.rs`，所以 fact/event 的 canary 的**唯一家**是本文件；与设计那句的差异记在
//! 交付报告里，不改旧文件。
//!
//! S3c-2 加 rows 窄审模板（`rows_review_*`）。它问的不是引文（fact/event 的行有
//! `quote`），也不是名字（naming 的锚），而是**一行记忆文本是否被它引用的那一轮撑得住**：
//! 主语对不对、数字有没有编、与既有事实是否矛盾、是不是只装了一件事。拍板 C1：证据只用
//! 行 `text` 对**证据轮**（`evidence_turn_uid` 那一轮）的正文，不做逐字引文——所以渲染只给
//! `text` 与 `TURN` 正文，没有 quote 可看。

use crate::batch::{Batch, ExtractCtx};
use crate::pass_event_contract::EventDraft;
use crate::pass_fact_contract::FactDraft;
use crate::prepare::prepare;
use crate::prompt::{assemble, shape_of, PromptShape};
use rc_contract::text::format_local_ms;
use rc_memory::writers_v4::A1Batch;

/// fact 模板的 canary。与 `prompt.rs` 的四条并存，但只在 fact 的 system 首行出现。
pub const FACT_CANARY: &str = "__FACT_CANARY_9d14__";

/// **审核**模板的 canary（S2b-2a）。审核与抽取是两次独立调用（设计 §4），所以 canary 也必须
/// 是两个：同一个串出现在两处，"这段输出是谁说的"就再也分不开。首行一次，只在审核 system。
pub const FACT_REVIEW_CANARY: &str = "__FACT_REVIEW_CANARY_5b27__";

/// **反判**（challenger）模板的 canary（工单 §7.2）。同一条 item 的第二次独立调用：
/// 自己不写行、不改行，只读 proposal 与引文，去找能让这条不成立/不持久的原文。
/// 与 [`FACT_REVIEW_CANARY`] 必须不同——同一串出现在两处，"这段输出是谁说的"就分不开。
pub const FACT_REVIEW_CHALLENGE_CANARY: &str = "__FACT_REVIEW_CHALLENGE_7e41__";

/// 审核契约的 `pass` 值。与抽取的 `PASS_FACT` 不同名：`pass` 列指向的是**哪一次调用**。
/// 两档（proposer / challenger）同形，所以两档共用这一格。
pub const PASS_FACT_REVIEW: &str = "fact_review";

/// 审核 `code` 的闭集（设计 §4「code 闭集」）。`pass` 的 code 必须是 null；
/// `reject` 必须带这五值之一，`uncertain` 可带其一也可为 null。值域的家是这里，parser 与 prompt 都从这一格取。
///
/// S2e-6：`wrong_subject` 是"引文撑得住，但主语不是做出这件事的人/组织/agent"
/// ——真实冒烟的发现 1：模型把 `项目记忆`/`阶段台账` 这类**来源**写成决定者，旧的四格
/// 没有一个词说得出这件事，审核只能判 pass。
pub const REVIEW_CODES: [&str; 5] =
    ["quote_mismatch", "not_durable", "missing_subject", "wrong_subject", "other"];

/// G1 刀 B（设计 `2026-11-08-g1-reuse-lesson-design.md` §波次一第 2 层）的**闭集问句**，
/// 逐字（Coze 口径）：价值轴只问这一件事，不问对错。
///
/// 它作为常量住在这里，是为了让"prompt 里那一句"与"测试钉的那一句"只有一个真相：
/// 渲染出的 system 必须逐字包含它（`tests/g1b_value_axis.rs` 钉着），改问句就得改常量。
pub const VALUE_AXIS_QUESTION: &str = "这一条会改变以后的回答吗？";

/// 价值轴的闭集取值（`value_axis` 这一格）。`keep` ⇒ 不盖章；`demote` ⇒ 落卡时盖
/// `reason='value_axis'` 的 archive 章（与程序预闸同表同待遇，先写者赢）。
/// 值域的家在这里：parser 与 prompt 都从这一格取，不各抄一份字面量。
pub const VALUE_AXIS_VALUES: [&str; 2] = ["keep", "demote"];

/// 价值轴的一格答案。**与三分判决正交**：`reject` 的行照样有一条价值轴答案，
/// 只是那行不落库，所以章也无处可盖（correctness 优先）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueAxis {
    Keep,
    Demote,
}

impl ValueAxis {
    pub fn as_str(self) -> &'static str {
        match self {
            ValueAxis::Keep => "keep",
            ValueAxis::Demote => "demote",
        }
    }

    /// 闭集解析：认不出就 `None`（不接受大小写变体、不接受同义词；parser 据此硬拒）。
    pub fn parse(raw: &str) -> Option<ValueAxis> {
        match raw {
            "keep" => Some(ValueAxis::Keep),
            "demote" => Some(ValueAxis::Demote),
            _ => None,
        }
    }
}

/// 设计 §2.2 的固定时区：v0 的 `turns` 没有事件时区，渲染一律按东八区。
pub const CN_TZ_OFFSET_MIN: i32 = 480;

// ── 压缩摘要块（S2e-6）：选批的隔离判据 ───────────────────────────────────────
//
// 真实冒烟的轮 9–12 起是 adapter 压出来的会话摘要（"This session is being continued…"）：
// 它们是**索引**，不是某人说过的话，把它当证据会让 fact/event 从摘要里"抽"出主语与引文。
// 选批在把这些轮拉进 batch 之前认出它们，落 `rejected(compact_summary)` 并跳过。
//
// 判据只认**块首的通用前缀**（六条），不认任何一次会话里的私有内容：
// - 适配器自带的标记 `[[compact_summary]]`（B 作为候选 adapter 的输出约定，见 `prompt.rs`
//   的同名常量——那边是"标记可独立成行"，这里是"块首"，两者服务不同的层）；
// - 旧摘要的英文开头句，大小写不敏感、允许前导空白；
// - 自动 checkpoint 的英文开头句（2026-10-31 在 DSH 语料实测：554 条非 system 轮里 4 条是这种
//   写法，角色是 `human`，选批的 system 过滤挡不住 ⇒ 它们被当成某人说过的话送进四个抽取 pass。
//   那一发 thinking 爆预算的长轮里也有一条就是它——见台账 §10.126）。

/// 压缩摘要块被拒收时写进 `extract_pass_state.reason` 的 slug。
pub const COMPACT_SUMMARY_REASON: &str = "compact_summary";

/// 块首前缀的闭集。加一条就是加一种**适配器约定**，不是加某一次会话的私有内容。
const COMPACT_PREFIXES: [&str; 6] = [
    "[[compact_summary]]",
    "This session is being continued from a previous conversation",
    "This is an automatically generated checkpoint condensing",
    // 后三条是 agent 应用**注入的上下文块**（DSH 实测 121/554 条 = 21.8%）。它们的共同点与压缩
    // 摘要一样：不是某人说过的话，而是拼进会话的背景快照。长度都在 notice 那道 240 字符闸之外
    // （429 / 1485 / 248 起），所以家在这一族而不是"短通知"那一族。
    "Current runtime context",
    "<system-reminder>",
    "Time sampled while preparing turn",
];

/// 这一段轮正文是不是压缩摘要块（S2e-6 的选批隔离判据）。
///
/// 只看**块首**：标记或那句英文出现在句子中间不算——讨论"这个标记"的普通轮不是摘要。
/// 大小写不敏感、允许前导空白；三个前缀全是 ASCII ⇒ 比较按字节走就是按字符走。
pub fn is_compaction_body(body: &str) -> bool {
    let text = body.trim();
    // `get(..n)` 在非字符边界上给 `None` ⇒ 短到切不出前缀的正文直接不匹配。
    COMPACT_PREFIXES
        .iter()
        // 逐字节切出等长前缀再比：`get(..n)` 落在非字符边界时给 `None` ⇒ 那一支不匹配
        // （短到切不出前缀的正文同样走 `None`）。
        .any(|prefix| {
            text.get(..prefix.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
        })
}

/// 运行期通知被拒收时写进 `extract_pass_state.reason` 的 slug（§10.66 方案 C）。
pub const NOTICE_REASON: &str = "not_utterance";

/// 已知的运行期通知前缀（**闭集**）。只收真实语料里见过、且是应用自己插的状态句；
/// 新增一条必须同时补一条用例（`runtime_notice_*`），不做正则、不做模糊匹配。
const NOTICE_PREFIXES: [&str; 3] = [
    "Model response interrupted",
    "Tool ran without output",
    "Command exceeded its timeout",
];

/// 通知句很短：以已知前缀开头、且整段不超过这个字符数才算（防止"正文里引用了通知"被误杀）。
const NOTICE_MAX_CHARS: usize = 240;

/// 这一段轮正文是不是 agent 应用插入的**运行期通知**（§10.62 缺陷 1 / §10.66 方案 C）。
///
/// 判据是"块首 + 短"：真实语料里被抽成事件的那一条正文是
/// `Model response interrupted. Completed output is preserved; automatic continuation stopped.` ——
/// 它既不是人说的、也不是 agent 自己写的话，不该进任何抽取 pass。
pub fn is_runtime_notice(body: &str) -> bool {
    let text = body.trim();
    if text.chars().count() > NOTICE_MAX_CHARS {
        return false;
    }
    NOTICE_PREFIXES.iter().any(|prefix| {
        text.get(..prefix.len())
            .map(|head| head.eq_ignore_ascii_case(prefix))
            .unwrap_or(false)
    })
}

/// `extract_pass_state.pass_id` 的这一格。`family` 才分三类。
pub const PASS_FACT: &str = "fact";

/// fact 契约的版本号：**不冒充 v4**（设计 §2 "新契约用 `schema_version=0`"）。
pub const FACT_SCHEMA_VERSION: u32 = 0;

/// 一次只问一个 family。`slug`/`category` 同值（DDL 闭集），`predicate` 是程序给的谓词闭集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FactFamily {
    Decision,
    Promise,
    Plan,
}

impl FactFamily {
    pub const ALL: [FactFamily; 3] = [Self::Decision, Self::Promise, Self::Plan];

    /// `family` 键与 `fact.category` 列的那一格。
    pub fn slug(self) -> &'static str {
        match self {
            Self::Decision => "decision",
            Self::Promise => "promise",
            Self::Plan => "plan",
        }
    }

    /// DDL 的 `fact.category` 闭集（`decision|promise|plan`）。v0 与 slug 同值。
    pub fn category(self) -> &'static str {
        self.slug()
    }

    /// v0 的谓词闭集：decision→`decide`、promise→`promise`、plan→`plan`。
    /// 动作细节放 `value`；扩展 family 才加词表（设计 §2.2）。
    pub fn predicate(self) -> &'static str {
        match self {
            Self::Decision => "decide",
            Self::Promise => "promise",
            Self::Plan => "plan",
        }
    }
}

/// 会话自锚的标记（C3）：候选表里"本 agent"的那一格。由 `append_agent_anchor` 盖章。
pub const MARK_SELF: &str = "self";

/// 主人锚的标记（C3）：候选表里"会话主人"的那一格。由 `append_principal_anchor` 盖章。
pub const MARK_PRINCIPAL: &str = "principal";

/// 一条候选锚的渲染视图。`reference` 是程序按 `candidates` 顺序分配的 `a1..aN`；
/// `anchor_id` 只给 parser/写库用，**不进 prompt**（模型没有抄它的入口）。
///
/// `mark`（C3）是**程序盖的身份章**，只两格：`self`（会话 `source_agent` 的自锚）与
/// `principal`（claim 解析出的主人锚）。`SPEAKER_NOTE` 用散文说"人轮的主人是主人锚、
/// agent 轮是本 agent"，候选表此前看不出是哪一格；渲染成 `- a3 | agent | qoder [self]`
/// 之后，模型不必从名字猜谁是谁。其余候选（含命名 pass 建的实体锚）`mark` 为 `None`，
/// 渲染不多一个字符。
#[derive(Debug, Clone)]
pub struct FactCandidate {
    pub reference: String,
    pub kind: String,
    pub canonical: String,
    pub anchor_id: String,
    pub mark: Option<&'static str>,
}

/// 候选行的唯一渲染点（fact 与 event 共用）：`- a3 | agent | qoder [self]`。
///
/// 标记只追加在行尾，**不改** `a* | kind | canonical` 三格的位置与分隔符：parser、审核
/// 和所有既有断言读的都是同一段文本。`kind`/`canonical` 走 `prepare`（与 TARGET 同一把
/// 剪刀）；`mark` 是程序常量，不过剪刀。
fn render_candidate_line(candidate: &FactCandidate, reference: &str) -> String {
    let mut line = format!(
        "- {reference} | {} | {}",
        prepare(&candidate.kind),
        prepare(&candidate.canonical)
    );
    if let Some(mark) = candidate.mark {
        line.push_str(&format!(" [{mark}]"));
    }
    line.push('\n');
    line
}

const HEAD_FACT: &str = "You are the fact pass of a memory center. This call asks for exactly one family: decision, promise or plan. You emit the commitments of that one family and nothing else: no events, no states, no relations, no moods, no one-off requests.\n";

const METHOD_FACT: &[&str] = &[
    // 台账 §10.46：t1 是"请阅读下面这样一段话"后面跟一整段【】文本，三族重放都问
    // "这算不算说话人自己的话"。判据落在 STEP 1，因为它决定"这一轮有没有要以第一人称读"。
    "STEP 1 read every TARGET turn once and ask it one question: does it state a durable decision, promise or plan with a subject that still matters later? A turn that pastes or quotes a block speaks in its own voice: first-person statements inside the pasted block belong to this turn's speaker unless the turn itself names who wrote the block.",
    // §10.46 两句：① object 的 acted upon 边界（导入某人的旧历史 ≠ 作用于该人）；
    // ② 混排子句里 value 的跨度（句子开头的指针/范围语不进 value，但 quote 仍可覆盖）。
    "STEP 2 pick the subject from the CANDIDATES a-refs; add an object a-ref only when the quote names the person or organization acted upon. A project, an event, a work item, a tool or a data source is not an object; leave object null. Being named only as the source or the destination of data (an old export, a session log, a vendor whose history is imported) is not being acted upon; leave object null there too. Put the action detail into value in the source's own words: value is the clause with only its leading subject and commitment verb removed (我们决定 / 我们计划 / 这个方案我们定了：), so the time expression stays inside value exactly as it is written, and nothing else may be dropped — never shorten it to a bare verb phrase and never leave out what will be done, for how long, or under which condition.",
    "STEP 3 copy quote character-for-character from that turn (no ellipsis, at most 120 characters). text is optional: if you write it, it is one plain sentence and every number in it must appear inside quote; if you are not sure, omit the key and the program writes one from the subject and the value.",
    "STEP 4 give every TARGET t-ref a home: either a fact whose turn or evidence names it, or empty_turns. A turn with nothing to record goes to empty_turns, never missing.",
];

const STANDARDS_FACT: &[&str] = &[
    // 台账 §10.26 缺陷 2：原句是 "still hold next week"，把本批核心（接下来 5 天 / 每天 /
    // 跑满 24 小时）全判成不够耐久。判据改成"会话结束后仍成立"，并明说短周期与周期性都算。
    "DURABLE AFTER THE SESSION: keep what is still true and still binding once this conversation ends. A plan for the next few days, a recurring schedule and a standing rule all count; a deadline inside a week is not a reason to drop a commitment. A greeting, a one-off request, a mood or a guess does not count.",
    // 台账 §10.26 重放：上一刀的 `ONE CLAUSE, ONE FACT` 没有定义"什么是一个子句"，
    // 且 agent 自述的会话内步骤被当成承诺（t17"我加一个小的 v1 出口"被 promise 记了、
    // plan/decision 没记）。这一条与 DURABLE AFTER THE SESSION 的分界只有一处：
    // 同一会话里马上要做的步骤 vs 跨会话仍然成立的规则/承诺/计划/决定。
    "WORK LOG IS NOT A COMMITMENT: an agent turn that narrates the step it is about to take in this same session (I will check X, I am now doing Y) is an operational step; keep that turn in empty_turns. A step whose only product is this session's artifact — a patch, an export, a script — is still a work log even though the artifact outlives the session; the event pass records it. This rule wins over the scope rule below when a clause narrates a step this session is executing. Only a rule, promise, plan or decision that outlives this conversation counts.",
    "ONE FAMILY ONLY: this call asks one family. family, category and predicate must be the three values printed in the output contract; anything else is a rejected answer.",
    "SUBJECT REQUIRED: subject is mandatory and is one of the CANDIDATES a-refs. When no candidate fits, do not guess: leave that turn in empty_turns.",
    // 台账 §10.26 缺陷 3：`subject 必须是候选` 与 `事实主体以引文为准` 同时在场、没有优先级。
    "SUBJECT PRIORITY: default to the speaker of the turn — the [self] anchor on agent turns, the [principal] anchor on human turns. Switch to another candidate only when the quote itself names a different third party as the one who decided, promised or planned. SUBJECT KIND: subject must be a person, an organization or an agent. An event, a work item or an object is never a subject; when no candidate of those kinds fits, put that turn in empty_turns instead of using an event anchor.",
    // 交接档 §3.2 规则 1（粒度四格之一）：一轮可以出多条，一条一个**子句**；出了 fact 的轮
    // 不许再进 empty_turns。长于 120 字符的子句与 VERBATIM 的上限在这里对齐。
    //
    // 台账 §10.26 重放（`f637921` 后三族重放：扮演模型的"必须猜"从 5 格变 6/8/9 格）：
    // 上一刀只写了"一条一个子句"却没定义子句，模型自己猜"冒号算不算、分号算不算、
    // `盯四件事——…` 算不算一条"（t1 被切成 7/6/7 条）。这里的 `CLAUSE BOUNDARY` 是
    // 可判定的一句：句末四个标点或换行才是边界，冒号/逗号/破折号都不是；同一子句内的
    // 列举不拆成多条。
    "CLAUSE BOUNDARY: a clause ends at 。 ！ ？ ； or a line break. Neither ： nor ， ends a clause, and a dash (——) does not either. ONE CLAUSE, ONE FACT: emit one fact per clause that carries a commitment; never split a list inside one clause into several facts. If one clause is longer than 120 characters, keep the shortest span that still carries the clause's action and its time or condition. A turn belongs in empty_turns only when it has no recordable clause at all; never put a turn that produced a fact into empty_turns.",
    // 台账 §10.44：`CLAUSE BOUNDARY` 只判死"在哪切"，没判"哪块算承诺"——三族重放里
    // 身份陈述、仓库状态、"某文档是唯一出处"、中心主机/路径各被记成一条 fact（gaps G1/G2/G6/G7）。
    // 这一条是承诺的正面判据 + 四类"不承载承诺"的闭集；最后一句把"常设禁令"从指针里摘出来。
    "COMMITMENT-BEARING CLAUSE: a clause carries a commitment only when it says what someone will do, has decided, promises, plans or forbids, and that still binds after this conversation. These clauses carry none: saying who someone is (I am the developer of X); reporting the current state of the work (a commit id, a green test count, what is being checked right now); a pointer that only says where information lives (document D is the only source for section S); narrating infrastructure (host, data path, open port). Emit no fact for them. A standing prohibition (do not reopen what was deferred) still counts as a commitment even when it sits next to such a pointer; an instruction that only scopes the current task does not. A clause that sets the scope or the operating rule of a named task or period counts as the plan's content and is kept: only do X this round, keep watching these four signals, stop and diagnose when any of them looks wrong, do not reopen what was deferred. Only a bare procedural instruction about the current tooling (read this file first, retry with that flag) is dropped. A statement of what is being verified right now is not scope: it stays under the current-state exclusion above.",
    // 交接档 §3.2 规则 2：quote 是连续一段，不跨句拼接；句末标点可留可去，借词不行。
    "VERBATIM QUOTE AND GROUNDED NUMBERS: quote is one contiguous verbatim span of the target turn, copied without ... or …, at most 120 characters. It must cover every character of value and must not splice two clauses: you may include or drop the punctuation that ends the clause, but never borrow words from another clause. Every digit or date inside text must be found in quote, so never invent a count, a price or a date. A turn that carries none of this belongs in empty_turns.",
];

/// 正例**按族分开**（台账 §10.26 缺陷 1）。此前三族共用一份 `POSITIVES_FACT`
/// （一条 promise 例 + 一条 decision 例），而尾段写死 `This call: family=.., category=..,
/// predicate=.. Any other value is rejected` ⇒ 每族都至少有一条示例违反自己的闭集
/// （plan 两族都不沾）。模板只留一支：句子随族换，JSON 的键与键序逐字相同，
/// `predicate` / `category` 两格由 [`FactFamily`] 填、**不许手写**。
///
/// 这条纪律由 `tests/pass_fact_contract.rs::examples_stay_inside_their_own_family` 逐条解析钉住：
/// 示例里出现本族以外的取值 ⇒ 红。示例用通用名与通用业务句，不抄语料原句。
fn positives_fact(family: FactFamily) -> [String; 2] {
    let fill = |line: &str| {
        line.replace("%FAMILY%", family.slug())
            .replace("%PREDICATE%", family.predicate())
    };
    let (first, second) = match family {
        FactFamily::Promise => (
            "GOOD: TARGET t1 says \"我下周五之前把对账单发给你\" and CANDIDATES has a1=张三 (person), a2=李四 (person) -> {\"ref\":\"f1\",\"turn\":\"t1\",\"subject\":\"a1\",\"predicate\":\"%PREDICATE%\",\"object\":\"a2\",\"value\":\"把对账单发给你\",\"category\":\"%FAMILY%\",\"mode\":\"stated\",\"time_text\":\"下周五之前\",\"quote\":\"我下周五之前把对账单发给你\",\"evidence\":[\"t1\"],\"text\":\"张三承诺在下周五之前把对账单发给李四。\"}",
            "GOOD: TARGET t2 says \"我会在周五下班前把验收清单发到群里\" and CANDIDATES has a1=王五 (person) -> {\"ref\":\"f2\",\"turn\":\"t2\",\"subject\":\"a1\",\"predicate\":\"%PREDICATE%\",\"object\":null,\"value\":\"把验收清单发到群里\",\"category\":\"%FAMILY%\",\"mode\":\"stated\",\"time_text\":\"周五下班前\",\"quote\":\"我会在周五下班前把验收清单发到群里\",\"evidence\":[\"t2\"],\"text\":\"王五承诺在周五下班前把验收清单发到群里。\"}",
        ),
        FactFamily::Decision => (
            "GOOD: TARGET t1 says \"我们决定先不上线，等迁移完\" and CANDIDATES has a1=某项目 (org) -> {\"ref\":\"f1\",\"turn\":\"t1\",\"subject\":\"a1\",\"predicate\":\"%PREDICATE%\",\"object\":null,\"value\":\"先不上线，等迁移完\",\"category\":\"%FAMILY%\",\"mode\":\"stated\",\"time_text\":\"等迁移完\",\"quote\":\"我们决定先不上线，等迁移完\",\"evidence\":[\"t1\"],\"text\":\"某项目决定先不上线，等迁移完成。\"}",
            "GOOD: TARGET t2 says \"这个方案我们定了：只改接口，不动数据结构\" and CANDIDATES has a1=某项目 (org) -> {\"ref\":\"f2\",\"turn\":\"t2\",\"subject\":\"a1\",\"predicate\":\"%PREDICATE%\",\"object\":null,\"value\":\"只改接口，不动数据结构\",\"category\":\"%FAMILY%\",\"mode\":\"stated\",\"time_text\":null,\"quote\":\"这个方案我们定了：只改接口，不动数据结构\",\"evidence\":[\"t2\"],\"text\":\"某项目决定只改接口，不动数据结构。\"}",
        ),
        FactFamily::Plan => (
            "GOOD: TARGET t1 says \"我们计划下周先跑一轮小流量，再决定要不要全量\" and CANDIDATES has a1=某项目 (org) -> {\"ref\":\"f1\",\"turn\":\"t1\",\"subject\":\"a1\",\"predicate\":\"%PREDICATE%\",\"object\":null,\"value\":\"下周先跑一轮小流量，再决定要不要全量\",\"category\":\"%FAMILY%\",\"mode\":\"stated\",\"time_text\":\"下周\",\"quote\":\"我们计划下周先跑一轮小流量，再决定要不要全量\",\"evidence\":[\"t1\"],\"text\":\"某项目计划下周先跑一轮小流量，再决定是否全量。\"}",
            "GOOD: TARGET t2 says \"接下来 5 天每天跑一次迁移校验，跑满 5 天再切流量\" and CANDIDATES has a1=某项目 (org) -> {\"ref\":\"f2\",\"turn\":\"t2\",\"subject\":\"a1\",\"predicate\":\"%PREDICATE%\",\"object\":null,\"value\":\"接下来 5 天每天跑一次迁移校验\",\"category\":\"%FAMILY%\",\"mode\":\"stated\",\"time_text\":\"接下来 5 天\",\"quote\":\"接下来 5 天每天跑一次迁移校验，跑满 5 天再切流量\",\"evidence\":[\"t2\"],\"text\":\"某项目计划接下来 5 天每天跑一次迁移校验，跑满 5 天再切流量。\"}",
        ),
    };
    [fill(first), fill(second)]
}

const NEGATIVES_FACT: &[&str] = &[
    "BAD: \"好的\" / \"收到\" / \"谢谢\" says nothing durable; those turns belong in empty_turns and produce no fact.",
    "BAD: \"他好像不太高兴\" is a mood or a guess, not a commitment; this pass never records it.",
    "BAD: \"我下周可能看看吧\" is a vague intention, not a promise, a decision or a plan; that turn belongs in empty_turns.",
    // 台账 §10.26 缺陷 2 的反面：一次性请求不是承诺——短周期是承诺，一次之差不是。
    "BAD: \"今天先帮我看一眼日志\" is a one-off request for a single action, not a commitment that outlives the conversation; it belongs in empty_turns even though it names a task and a time.",
    // 台账 §10.44：三类"看似有信息、实为状态/指针/设施"的子句，是上一轮三族重放里
    // 过度抽取的来源（t1 的身份、HEAD 与绿数、"唯一出处"、中心主机/路径）。
    "BAD: \"当前版本 1200 个测试全绿\" only reports the state of the work at this moment; it carries no commitment and produces no fact, and the other clauses of that turn still count on their own.",
    "BAD: \"这些读数只在那份文档里记过\" only says where information lives; a bare pointer produces no fact (a standing prohibition in the same clause still counts).",
    "BAD: \"部署机地址是 10.0.0.5，数据盘在 /data，端口已放行\" narrates infrastructure; it is not a decision, a promise or a plan.",
];

/// 输出契约尾段。`%FAMILY%` / `%PREDICATE%` 是这一调用固定的两个词，逐字替换；
/// 这样"逐字给出 JSON 形状"与"一次只问一个 family"在同一段文本里同时成立。
const TAIL_FACT: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly five keys, all required: schema_version, pass, family, facts, empty_turns. schema_version is 0 and pass is \"fact\".
{\"schema_version\":0,\"pass\":\"fact\",\"family\":\"%FAMILY%\",\"facts\":[],\"empty_turns\":[]}
This call: family=%FAMILY%, category=%FAMILY%, predicate=%PREDICATE%. Any other value is rejected.
facts[]: {\"ref\",\"turn\",\"subject\",\"predicate\",\"object\",\"value\",\"category\",\"mode\",\"time_text\",\"quote\",\"text\",\"clause\",\"evidence\"}. ref is unique in this reply; turn is a TARGET t-ref; subject and object are CANDIDATE a-refs (object may be null); at least one of object and value is non-null; mode is always \"stated\"; time_text copies the time expression exactly as it is written in the quote, including a verb or prefix that belongs to it (接下来 5 天, 每天, 跑满 24 小时, 本轮); never shorten it to a bare number, never add words that are not in the quote; when the quote has no time expression, write null; never borrow a time from another clause or from the turn timestamp (never write later, soon or a guessed date); quote is the verbatim substring; evidence is a non-empty list of TARGET t-refs that contains turn. text may be omitted; give clause (the c-ref printed under the target) and omit quote — the program copies that clause as the quote.
empty_turns[]: TARGET t-refs with no commitment in them, each at most once.
Do not write any id, uid, anchor_id, *_ms field, confidence, occurred_at or due_at. Extra keys such as occurred_at_ms, due_at_ms, confidence, evidence_turn_uid or turn_uid are rejected. Do not invent a t-ref or an a-ref. Text stays in the language of the source.";

fn fact_tail(family: FactFamily) -> String {
    TAIL_FACT
        .replace("%FAMILY%", family.slug())
        .replace("%PREDICATE%", family.predicate())
}

/// system = canary（首行一次）+ 方法四步 + 标准八条 + 本族正例 2 + 反例 7 + 输出契约尾段。
/// 标准第二条（`WORK LOG IS NOT A COMMITMENT`）与第六条（子句边界并进 `ONE CLAUSE, ONE FACT`）
/// 是 `f637921` 三族重放的产物：前者钉"会话内步骤不是承诺"，后者钉"子句在哪里结束"。
/// §10.47：**弱模型档**。`RECOLLECT_FACT_PROMPT=compact` 时 fact 用这一份 system。
///
/// 由来（Qwen3-4B 实测）：8 条标准的完整栈对 4B 太长，它只记第一条、把整轮并成一条 fact；
/// 换成"程序切好的子句 + 只回 clause id"之后，它在同一批上能稳定枚举出 7 条。
/// 三条要点：① 只回 `clause`，quote/value/time_text 由程序从子句与说话人填（不重打、不编）；
/// ② 判定只剩一张"留/丢"清单；③ 输出契约与完整档同形，parser 不需要第二个解析器。
pub fn fact_system_compact(family: FactFamily) -> String {
    format!(
"{FACT_CANARY}
逐个判断下面的子句是不是这一类承诺（family = {family}）。
承诺 = 说话人以后要做的事、已经定下的事、或以后不许做的事，并且在这段对话结束后仍然有用。
不是承诺：自我介绍；现在的工作状态（改了哪些文件、测试绿不绿、正在查什么）；只说文档或报告在哪；只说主机、路径、端口、防火墙状态；agent 这一轮马上要执行的步骤；寒暄。
带禁令（不要重开、只读某份）的算承诺，哪怕句子开头是文件路径。
带周期性或期限（每天、每周、接下来几天、跑满 24 小时）并说了要做的事，算承诺。
只说中心/服务器地址、数据盘路径、端口放行这类环境配置的，不算承诺，哪怕动词是「已放行」。

输出 JSON，每个 cN 都要有一条，按顺序：
{{\"marks\":[{{\"c\":\"c1\",\"keep\":false}},{{\"c\":\"c2\",\"keep\":true}}]}}
只输出 JSON。",
        FACT_CANARY = FACT_CANARY,
        family = family.slug()
    )
}

/// 运行时选档：`RECOLLECT_FACT_PROMPT=compact` ⇒ 弱模型档；其余一律完整档。
/// 测试与其余调用点仍走纯函数 [`fact_system`]，环境变量只在 runner / dry-run 这一处读。
pub fn fact_system_selected(family: FactFamily) -> String {
    match std::env::var("RECOLLECT_FACT_PROMPT").as_deref() {
        Ok("compact") => fact_system_compact(family),
        _ => fact_system(family),
    }
}

pub fn fact_system(family: FactFamily) -> String {
    let tail = fact_tail(family);
    let positives = positives_fact(family);
    let positives: Vec<&str> = positives.iter().map(String::as_str).collect();
    assemble(FACT_CANARY, HEAD_FACT, &tail, METHOD_FACT, STANDARDS_FACT, &positives, NEGATIVES_FACT)
}

pub fn fact_shape() -> PromptShape {
    // 三族正例条数相同（模板一支），形状只数条数。
    let positives = positives_fact(FactFamily::Promise);
    let positives: Vec<&str> = positives.iter().map(String::as_str).collect();
    shape_of(METHOD_FACT, STANDARDS_FACT, &positives, NEGATIVES_FACT)
}

/// 身份层 I2 的**说话人说明**（设计 §3.3）：TARGET 段后面固定的一句。
///
/// 它回答的只有一件事——"每一轮的 role 是谁在说话"，而这个答案由程序给（`turns.role`
/// 与会话的主人锚），不让模型从正文推断。模型能做的仍然只是"在候选 `a*` 里选主语"。
/// 这一句逐字固定在两个 `*_user` 里，改它就是改两个 pass 的输入指纹（同一条纪律）。
pub const SPEAKER_NOTE: &str = "说话人说明：human 轮的说话人是会话主人（候选表里 kind=person 且与会话主人绑定的那一格）；agent 轮的说话人是本 agent。说话人只是默认主语；只有引文本身点名了另一个人或组织时才换主语，引文里没有可挂的主语时不要把 event/work 之类顶上。";

/// §10.47：`SPEAKER_NOTE` 是工单里的逐字字面量（`prompt_shape` 钉着不许改写），
/// 所以"TARGET 行上的 speaker= 怎么读"单独用一句补在后面，不去动那一句。
pub const SPEAKER_LINE_NOTE: &str = "TARGET 行末尾的 `speaker=a*` 是程序解析出的说话人短 id：有这一格，它就是该轮的默认主语，不必再从候选表反推。";

/// §10.47：子句由程序切好并编号，模型只判"留不留"。
pub const CLAUSE_NOTE: &str = "编号子句：每个 TARGET 下面列出它的 `[cN]` 子句（程序已按句末标点与换行切好）。出 fact 时只给 `clause`（例如 \"c7\"），程序会把 quote 抄成该子句原文——给了 clause 就可以省略 quote；不要自己改写字句。";

/// 程序切子句（§10.47）：句末四个标点（。！？；）或换行收一句，标点留在该句末尾。
/// 与 `CLAUSE BOUNDARY` 的标准同一把尺子——弱模型不再需要自己判"哪里算一句"。
pub fn split_clauses(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        current.push(ch);
        if matches!(ch, '。' | '！' | '？' | '；' | '\n') {
            let clause = current.trim();
            if !clause.is_empty() {
                out.push(clause.to_string());
            }
            current.clear();
        }
    }
    let clause = current.trim();
    if !clause.is_empty() {
        out.push(clause.to_string());
    }
    out
}

/// 一条编号子句：渲染与解析共用同一份枚举（`c1..cN` 按 `batch.rows` 顺序、行内顺序）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClauseRef {
    pub id: String,
    pub row: usize,
    pub text: String,
}

/// 整批的编号子句表。**渲染与 parser 都只调这一个函数**：各写一份就会让模型看到的
/// `c7` 与 parser 认的 `c7` 分叉（与 `a*` 短 id 同一条纪律）。
pub fn clause_index(batch: &Batch) -> Vec<ClauseRef> {
    let mut out = Vec::new();
    let mut next = 1usize;
    for (row, turn) in batch.rows.iter().enumerate() {
        for text in split_clauses(&prepare(&turn.body)) {
            out.push(ClauseRef { id: format!("c{next}"), row, text });
            next += 1;
        }
    }
    out
}

/// 每轮说话人的短 id（程序解析，§10.47）：agent 轮取带 [`MARK_SELF`] 的候选；
/// human 轮取带 [`MARK_PRINCIPAL`] 的候选，没有 claim 时退到**批内唯一的 person 候选**。
/// 解析不出来（多个 person 且无 claim）就返回 `None`——宁可不写，也不把主语猜错；
/// 那一格由 [`SPEAKER_NOTE`] 的散文兜底。
///
/// 弱模型的真跑（Qwen3-4B）：human 轮上它取了 `a1 [self]`（agent 自锚）当主语，
/// 因为它只能从候选表的标记里反推"谁是说话人"。把这一步做成程序输出，模型就只剩"选"。
pub(crate) fn speaker_ref(role: &str, candidates: &[FactCandidate]) -> Option<String> {
    let mark = if role == "agent" { MARK_SELF } else { MARK_PRINCIPAL };
    if let Some(index) = candidates.iter().position(|candidate| candidate.mark == Some(mark)) {
        return Some(format!("a{}", index + 1));
    }
    if role == "agent" {
        return None;
    }
    let persons: Vec<usize> = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| candidate.kind == "person")
        .map(|(index, _)| index)
        .collect();
    if persons.len() == 1 {
        return Some(format!("a{}", persons[0] + 1));
    }
    None
}

/// 这一轮的说话人短 id（D 子 agent 接线）：优先用程序读到的**轮级说话人锚**
/// （[`crate::batch::TurnRow::speaker_anchor_id`]），按 `anchor_id` 在候选里找位置；
/// 找不到（或这一轮没有说话人 mention）⇒ **逐字回落到** [`speaker_ref`] 的 role 推导。
///
/// 按 id 而不是 canonical 找：同一 canonical 在不同 scope 可以是两格，按名字找会撞。
/// 渲染层不猜：候选里没有这一格（例如锚已退场、没进补位）就不写 `speaker=` 的覆盖值。
pub(crate) fn turn_speaker_ref(
    speaker_anchor_id: Option<&str>,
    role: &str,
    candidates: &[FactCandidate],
) -> Option<String> {
    if let Some(anchor_id) = speaker_anchor_id {
        if let Some(index) =
            candidates.iter().position(|candidate| candidate.anchor_id == anchor_id)
        {
            return Some(format!("a{}", index + 1));
        }
    }
    speaker_ref(role, candidates)
}

/// 目标轮的时间：分钟级、固定 `+08:00`，不出现毫秒。
fn render_target_ts(ms: i64) -> String {
    format!("{}+08:00", format_local_ms(ms, CN_TZ_OFFSET_MIN).replace(' ', "T"))
}

// ── 抽取输入的 CTX 段（C6b）：只补上下文，不改指令 ───────────────────────────
//
// 抽取模型此前只见 `TARGET [t*]`：语式（"要是当时…"是不是假设）、条件（"确认后"跟谁走）、
// 指代（"那个""刚才说的"指哪一句）在单轮里无从判断。这一层把目标轮**同会话**
// `created_at` 前后各一轮渲进 `CTX` 段，并明说它不是证据。
//
// 三条纪律（派发档 `2026-10-28-ctx-render`）：
// 1. 段头一句固定说明（[`CTX_NOTE`]），位于全部 `TARGET` 之前；没有邻轮时明写
//    `CTX: (none)`，不静默省略；
// 2. CTX 轮**不是证据**：`evidence` 仍只收 `t*`，`c*` 不是引用面，也不进 `pass_state`
//    与读数；
// 3. 压缩摘要块只允许出现在这一段（选批已把它们挡在 `TARGET` 外，见 `is_compaction_body`），
//    出现在 TARGET 仍按既有规则拒。

/// CTX 段的段头句（C6b）。**固定一句**，不随批、不随 pass 变：它是边界说明，不是提示排版。
pub const CTX_NOTE: &str =
    "以下邻轮只是上下文，只用于判断语式、条件与指代，不得作为证据，不得引用其中内容。";

/// CTX 段 = `CTX (not evidence)` + [`CTX_NOTE`] + 逐条 `CTX [c*]`；没有邻轮 ⇒ `CTX: (none)`。
///
/// `c*` 按**渲染顺序**编号（与 `t*`/`a*` 的"位置即契约"同一条纪律），它只用来定位邻轮；
/// 模型若把它写进 `evidence`，contract 按 `evidence_not_target` 拒（`c*` 不在目标表里）。
fn render_extract_ctx(s: &mut String, batch: &Batch, ctx: &ExtractCtx) {
    if ctx.is_empty() {
        s.push_str("CTX: (none)\n");
        return;
    }
    s.push_str("CTX (not evidence)\n");
    s.push_str(CTX_NOTE);
    s.push('\n');
    for (index, turn) in ctx.turns.iter().enumerate() {
        // 邻轮挨着哪一轮由位置换算（t1..tN = batch.rows 顺序），不改 TARGET 段的编号。
        let target = batch
            .rows
            .iter()
            .position(|row| row.turn_uid == turn.target_turn_uid)
            .map(|at| format!(" t{}", at + 1))
            .unwrap_or_default();
        let mark = if is_compaction_body(&turn.row.body) { " [compact summary]" } else { "" };
        s.push_str(&format!(
            "CTX [c{}] {}{}{} {} {}: {}\n",
            index + 1,
            turn.side.label(),
            target,
            mark,
            render_target_ts(turn.row.created_at_ms),
            prepare(&turn.row.role),
            prepare(&turn.row.body)
        ));
    }
}

/// user = `PASS fact v0 | family <slug>` 首行 + CTX 段 + 逐轮 TARGET + 候选锚。
///
/// - CTX（C6b）：目标轮同会话的前后各一轮，位于全部 TARGET **之前**；只作上下文，
///   见 [`render_extract_ctx`]；
/// - TARGET 的编号是 `batch.rows` 顺序（`t1..tN`），`role` 与正文都先过 `prepare`
///   （H6：同一个格子里的每个字一律走这把剪刀，不因"取值受 ingest 约束"就绕开）；
/// - TARGET 段后面固定接一句 [`SPEAKER_NOTE`]（I2）：说话人是谁由程序说，模型只选主语；
/// - CANDIDATES 只渲染 `a* | kind | canonical`，`anchor_id` 一个字都不出现；C3 起
///   自锚/主人锚在行尾多一个 `[self]` / `[principal]` 标记（其余候选不变）；
/// - 候选为空时明说 `(none in this batch)`：模型不该在一个空表上猜。
pub fn fact_user(
    batch: &Batch,
    candidates: &[FactCandidate],
    family: FactFamily,
    ctx: &ExtractCtx,
) -> String {
    let mut s = String::from("PASS fact v0 | family ");
    s.push_str(family.slug());
    s.push('\n');
    render_extract_ctx(&mut s, batch, ctx);
    let clauses = clause_index(batch);
    for (index, row) in batch.rows.iter().enumerate() {
        let role = prepare(&row.role);
        let speaker = speaker_ref(&row.role, candidates)
            .map(|reference| format!(" speaker={reference}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "TARGET [t{}] {} {}{}: {}\n",
            index + 1,
            render_target_ts(row.created_at_ms),
            role,
            speaker,
            prepare(&row.body)
        ));
        for clause in clauses.iter().filter(|clause| clause.row == index) {
            s.push_str(&format!("  [{}] {}\n", clause.id, clause.text));
        }
    }
    s.push_str(SPEAKER_NOTE);
    s.push('\n');
    s.push_str(SPEAKER_LINE_NOTE);
    s.push('\n');
    s.push_str(CLAUSE_NOTE);
    s.push('\n');
    s.push_str("CANDIDATES (subject/object 只能引用这些 a*):\n");
    if candidates.is_empty() {
        s.push_str("(none in this batch)\n");
    }
    for (index, candidate) in candidates.iter().enumerate() {
        // 批内短 id 由**位置**决定（设计 §2.2："a1..aN = candidates 顺序"），渲染与
        // parser 都从这里取，不各读一份。`reference` 必须等于同一格，否则是调用方的错。
        let reference = format!("a{}", index + 1);
        debug_assert_eq!(candidate.reference, reference);
        s.push_str(&render_candidate_line(candidate, &reference));
    }
    s
}

// ── 审核（S2b-2a）：与抽取分开的第二次调用 ────────────────────────────────────
//
// 审核只问**一个问题**（S2e-6 起这一问有三格）：引文是否支持这条的主语/谓词/值/类别、
// 它是否是耐久承诺（不是一次性请求/寒暄/推测），**以及主语是不是做出这件事的人/组织/agent**
// ——"项目记忆""阶段台账""文档"这类来源不是行动主体。它不改写、不重抽、不造 id，也只读
// 「行 + 引文 + 目标轮」：生成理由、confidence、别的草稿一律不给——给了就是让模型给自己的输出打分。

/// R2-1：**空邻居池不得被读成"没有矛盾"**。challenger 用判决版、audit 票 3 用布尔版
/// （[`ABSENT_NEIGHBORS_HOLDS`]）；fact proposer 与 event 两份**必须没有**（它们没有矛盾轴）。
/// 判决版的唯一引用点是 challenger 的 STANDARDS，由 `audit_user` 的
/// `NEIGHBORS: unavailable …` 那一行供料。
///
/// 只改**信息 + 判据**，不加硬闸：程序侧仍按模型的判决收下（空池本身不等于 reject）；
/// 模型读不到邻居时必须答 uncertain，而不是拿"下方没有邻居"当"没有矛盾"。
pub const ABSENT_NEIGHBORS_NOTE: &str = "ABSENCE OF NEIGHBORS IS NOT EVIDENCE OF NO CONTRADICTION: when the items below say the neighbor pool is unavailable, you cannot verify that nothing contradicts this row. If the row's support depends on that comparison, answer uncertain; never answer pass because the pool was empty.";

/// 同一条语义的**布尔契约**版本：audit 的三票只有 `holds`，没有 uncertain 这一态。
/// 判不了就落 `holds=false`（下游降级为 Low/Failed），绝不能因为池子是空的而答 `true`。
pub const ABSENT_NEIGHBORS_HOLDS: &str = "ABSENCE OF NEIGHBORS IS NOT EVIDENCE OF NO CONTRADICTION: when the items below say the neighbor pool is unavailable, you cannot verify that nothing contradicts this row. If the row's support depends on that comparison, answer holds=false; never answer true because the pool was empty.";

const HEAD_FACT_REVIEW: &str = "You are the review pass of a memory center. This is a second, independent call: you did not write these rows and you never rewrite them. For each row answer exactly this: does the quote support its subject, predicate, object or value and category; is it a durable commitment rather than a one-off request, a greeting or a guess; and WHO DID IT — is the subject the person, organization or agent that actually made this decision, promise or plan, rather than a memory store, a project memory, a stage ledger, a log or a document that only reports it? Alongside that verdict you answer one further question about the same row, on its own axis: 这一条会改变以后的回答吗？ Would recalling this row later change what someone answers or does — keep — or is it a work log that only records what was already done — demote.\n";

const METHOD_FACT_REVIEW: &[&str] = &[
    "STEP 1 read one ITEM at a time. An item is a row someone else extracted: its subject, predicate, object or value, category, its quote, and the target turn the quote was copied from.",
    "STEP 2 read only the quote and the target turn below it, and answer the first half of the question: does the quote, character for character, support this subject, this predicate, this object or value and this category?",
    "STEP 3 answer the second half: WHO DID IT. The subject must be the actor — the person, organization or agent that took the decision, made the promise or set the plan. A memory store, a project memory, a stage ledger, a log or a document is where the claim was read, not the one who did it.",
    "STEP 4 answer the third part: is it a commitment that still holds once this conversation ends — including plans for the next few days and recurring schedules — or a one-off request, a greeting or a guess? Then give each item exactly one verdict: pass when every part holds, reject when any fails, uncertain when the quote alone cannot settle it. You never repair a row and never extract a new one.",
    // G1 刀 B：价值轴是**第二次判断的同一行**，但它不改上面那一半的对错。问句逐字
    // （[`VALUE_AXIS_QUESTION`]），答案只两格（[`VALUE_AXIS_VALUES`]）。
    "STEP 5 answer the value axis for every item, as a question of its own: 这一条会改变以后的回答吗？ Ask whether this row would change what an agent answers or does a month from now. A decision, a promise, a plan, a deadline, a standing rule or a constraint that binds later work changes later answers — keep. A work log that only records a step already taken (a command run, a status check, a one-off verification, a restart, a file written) changes no later answer — demote. Write value_axis keep or demote on every item, including the ones whose verdict is reject or uncertain: correctness and value are two axes and neither is read from the other.",
];

const STANDARDS_FACT_REVIEW: &[&str] = &[
    "THE QUOTE IS THE WHOLE EVIDENCE: judge the characters of the quote, not what the speaker probably meant and not what a careful reader could add from context.",
    // 工单 §7.2：判决必须带证据。没证据的 pass/reject 在程序侧一律降 uncertain，
    // 所以这里要让模型知道"不给证据 = 这个判决不算数"。
    "EVERY VERDICT CITES THE QUOTE: pass and reject both carry evidence — one contiguous run of characters copied from that item's own quote that supports the verdict. evidence is not a summary and not a paraphrase; a verdict without evidence is not a verdict. uncertain may carry null only when the quote alone cannot settle the question.",
    "NO REPAIR, NO NEW ROWS: never rewrite a quote, a value or a subject, never merge two items, never add an item that is not in this call. Your only output is one verdict per item.",
    "DURABLE ONLY: a greeting, a one-off request, a mood or a guess is not a commitment and is rejected with not_durable. Durability means it still holds after this conversation ends — a plan for the next few days or a recurring schedule passes; a short deadline is not a reason to reject. A vague intention the quote does not pin down is uncertain.",
    "CODES ARE CLOSED: quote_mismatch when the quote does not carry the subject, predicate, value or category; missing_subject only when neither the quote nor the `speaker:` line of that TARGET TURN identifies an actor — a clause of that turn that names no other actor inherits the turn's speaker, so its subject is supported; wrong_subject when the subject is a source such as a memory store, a project memory, a stage ledger, a log or a document instead of the person, organization or agent that did it; not_durable when it is not a durable commitment; other for anything else. pass always carries code null; uncertain may carry null or one of these codes.",
    // 台账 §10.26 缺陷 4：此前"自述句"只要两半都成立就过，等于把发言人的自称当成第二证人。
    "SELF-REPORT IS NOT A SECOND WITNESS: when the row rests on the speaker reporting their own decision, promise or plan, the quote itself must still carry the concrete content — who committed to what, and the time or condition it holds for. A bare \"已完成\" / \"我会处理\" / \"done\" with no object and no timeframe is quote_mismatch, not pass.",
    // 台账 §10.49：抽取侧已把"工作日志不是承诺"写死在程序里，审核侧此前没有这一轴——
    // MiniMax 对"核对当前树…再决定下一步"这类草稿一律放行，等于这条闸对过度抽取无效。
    "WORK LOG IS NOT A DURABLE COMMITMENT: a row whose quote narrates the step the speaker is taking in this same session (I will check X, I am now doing Y, let me dig into Z) is an operational step, not a commitment that outlives the conversation — reject it with not_durable even when it names an object, a condition or a document.",
    // G1 刀 B：价值轴与对错轴**各判各的**。这一条钉三件事：轴不互相改写、demote 不是
    // 另一种 reject、keep 不是 pass 的同义词。
    "VALUE AXIS IS ITS OWN AXIS: value_axis answers only 这一条会改变以后的回答吗？ — would this row change a later answer. Demote a row that only logs a step already taken, even when its verdict is pass. Keep a decision, a promise, a plan, a deadline or a standing rule, even when you reject it for a correctness reason. Never demote for quote_mismatch and never pass for value: the verdict and the axis are written separately and the program treats them separately.",
    // 邻居池那段只属于有"矛盾轴"的 pass（challenger 与 audit）。这一份判的是
    // "引文是否支持这一行 / 是否持久 / 谁做的"，没有矛盾维度，带上它只会逼出多余的 uncertain。
];

const POSITIVES_FACT_REVIEW: &[&str] = &[
    // 台账 §10.52：`value` 与 quote 必须逐字一致（object 仍是李四）。
    "GOOD: ITEM f1: subject 张三, predicate promise, object 李四, value 把对账单发给你, category promise, quote \"我下周五之前把对账单发给你\" -> the quote names the subject, the commitment and the time, Zhang San is the one who promised, and it is durable; it also still changes a later answer, so the value axis is keep; evidence is copied from the quote: {\"ref\":\"f1\",\"verdict\":\"pass\",\"code\":null,\"evidence\":\"把对账单发给你\",\"value_axis\":\"keep\"}",
];

const NEGATIVES_FACT_REVIEW: &[&str] = &[
    "BAD: ITEM f2 quotes \"好的，收到\": a greeting commits to nothing durable, so the item is rejected with the words of the quote as evidence: {\"ref\":\"f2\",\"verdict\":\"reject\",\"code\":\"not_durable\",\"evidence\":\"好的，收到\"}",
    "BAD: ITEM f3 claims value 把对账单发给李四 but its quote reads \"下周再聊\" and names neither the object nor the action: the quote does not support the row: {\"ref\":\"f3\",\"verdict\":\"reject\",\"code\":\"quote_mismatch\",\"evidence\":\"下周再聊\"}",
    "BAD: ITEM f4 has subject 项目记忆 and its quote reads \"项目记忆显示，阶段台账计划再核实一遍\": a memory store is where the claim was read, not the person or agent who planned it, so the subject is wrong: {\"ref\":\"f4\",\"verdict\":\"reject\",\"code\":\"wrong_subject\",\"evidence\":\"项目记忆显示\"}",
    "BAD: ITEM f5 says value 把迁移做完 but its quote is only \"已经处理好了\": the quote carries no object and no timeframe, so nothing about the commitment can be checked: {\"ref\":\"f5\",\"verdict\":\"reject\",\"code\":\"quote_mismatch\",\"evidence\":\"已经处理好了\"}",
    "BAD: ITEM f6 quotes \"我先核对当前树和审核报告，再决定下一步\": it narrates a step this session is taking, not a commitment that outlives the conversation, so it is rejected: {\"ref\":\"f6\",\"verdict\":\"reject\",\"code\":\"not_durable\",\"evidence\":\"再决定下一步\"}",
    // G1 刀 B：价值轴的反例。这一行**对**（引文支持、耐久）但**没有价值**——它只记录
    // 一次已经做过的核对。价值轴写 demote，判决两轴分开写。句子是自编的通用流水句。
    "BAD (value axis): ITEM f7 quotes \"服务监控已经启动，日志在输出目录里\" and the verdict is pass, but recalling it a month from now changes no answer: the value axis is demote, and it is written next to the verdict, not instead of it: {\"ref\":\"f7\",\"verdict\":\"pass\",\"code\":null,\"evidence\":\"服务监控已经启动\",\"value_axis\":\"demote\"}",
    // 反面的反面：价值轴不是"再判一次对错"。对错上判 uncertain 的行，价值轴照样给结论。
    "BAD (axis leaking into the verdict): answering reject only because the row is a work log would be wrong when the quote does support it — a work log is demoted, not rejected; conversely, answering value_axis keep because the row passed is wrong when nothing later depends on it.",
];

/// 审核的输出契约。三段判决的样例逐字给出（设计 §4 "输出" 那一格），`%FAMILY%` 逐字替换。
///
/// 工单 §7.2：`verdicts[]` 每条四键，`evidence` 必须是该条 `quote` 里的一段连续原文。
/// `pass`/`reject` 缺证据由程序降 `uncertain`（不是整批拒）；`uncertain` 允许 null。
const TAIL_FACT_REVIEW: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly four keys, all required: schema_version, pass, family, verdicts. schema_version is 0 and pass is \"fact_review\".
{\"schema_version\":0,\"pass\":\"fact_review\",\"family\":\"%FAMILY%\",\"verdicts\":[{\"ref\":\"f1\",\"verdict\":\"pass\",\"code\":null,\"evidence\":\"把对账单发给你\",\"value_axis\":\"keep\"},{\"ref\":\"f2\",\"verdict\":\"reject\",\"code\":\"not_durable\",\"evidence\":\"好的，收到\",\"value_axis\":\"demote\"},{\"ref\":\"f3\",\"verdict\":\"reject\",\"code\":\"quote_mismatch\",\"evidence\":\"下周再聊\",\"value_axis\":\"keep\"}]}
This call: family=%FAMILY%. Every item below belongs to this family; any other family value is rejected.
verdicts[]: one entry per ITEM, {\"ref\",\"verdict\",\"code\",\"evidence\",\"value_axis\"}. ref is the ITEM f-ref printed for that row and appears exactly once. verdict is one of pass, reject, uncertain. code is null when verdict is pass, and uncertain may also carry null when the quote alone cannot settle the question; otherwise code is one of quote_mismatch, not_durable, missing_subject, wrong_subject, other. A subject that is a memory store, a project memory, a stage ledger, a log or a document is not the actor: reject it with wrong_subject.
value_axis answers 这一条会改变以后的回答吗？ and is exactly one of keep or demote, written for every item whatever its verdict; any other value is rejected, and a missing value_axis is read as keep.
evidence is required for pass and reject and must be one contiguous run of characters copied from that item's own quote, never a summary or a paraphrase; uncertain may carry null. A pass or reject without evidence, or with evidence that is not inside the quote, is read as uncertain.
Do not write any identity, turn reference, anchor reference, timestamp or millisecond value. Extra keys are rejected. Do not add items, do not drop items, do not rewrite a row. Text stays in the language of the source.";

/// system = canary（首行一次）+ 方法五步 + 标准八条 + 正例 1 + 反例 7 + 输出契约尾段。
/// 第 5 步是 G1 刀 B 的价值轴（与对错轴各判各的，见 `METHOD_FACT_REVIEW` 末条）。
/// 形状与 [`fact_system`] 共用 `assemble`：审核不是"另一套模板框架"，只是另一次调用。
pub fn fact_review_system(family: FactFamily) -> String {
    let tail = TAIL_FACT_REVIEW.replace("%FAMILY%", family.slug());
    assemble(FACT_REVIEW_CANARY, HEAD_FACT_REVIEW, &tail, METHOD_FACT_REVIEW,
             STANDARDS_FACT_REVIEW, POSITIVES_FACT_REVIEW, NEGATIVES_FACT_REVIEW)
}

pub fn fact_review_shape() -> PromptShape {
    shape_of(METHOD_FACT_REVIEW, STANDARDS_FACT_REVIEW, POSITIVES_FACT_REVIEW,
             NEGATIVES_FACT_REVIEW)
}

// ── 反判（challenger，工单 §7.2）：同一条 item 的第二次独立调用 ─────────────────
//
// 角色只有一句话：**只依据 quote，去找能让这条不成立/不持久的原文**。
// 找到 ⇒ reject + code + 那段原文；找不到任何反证 ⇒ pass + 支撑该结论的原文片段；
// quote 太短/两可 ⇒ uncertain。它不改写、不新增行、不引用块外内容。
//
// 两档同形：输出契约与 proposer 完全一致（四键 verdicts、code 闭集、`pass` 值同为
// `fact_review`），差别只在 system 的角色与方法。user 段两档共用（同一片、同一套 ref），
// 这样"两次调用看的是同一份输入"在程序侧是同一个字符串，不靠人记得对齐。

const HEAD_FACT_REVIEW_CHALLENGE: &str = "You are the challenge pass of a memory center. This is a second, independent call about the same items: you did not write them, you never rewrite them, and you never propose a replacement. Your only job is adversarial and narrow: reading ONLY the quote of an item, look for the words already written in that quote that would make the item fail or stop holding. Found counter-evidence -> reject. No counter-evidence anywhere in the quote -> pass, and cite the words that support that conclusion. Quote too short or genuinely two-way -> uncertain. You never repair a row, never add an item, and never cite a character that is not inside the quote.\n";

const METHOD_FACT_REVIEW_CHALLENGE: &[&str] = &[
    "STEP 1 read one ITEM at a time: its subject, predicate, object or value, category, its quote, and the target turn printed below the quote. The quote is the only evidence; the target turn and CTX are context, never evidence.",
    "STEP 2 search inside the quote itself for words that defeat the item: a different object or value than the row claims, a condition that may not hold, a hedge that withdraws or postpones the commitment, a greeting or a one-off request instead of a durable commitment, or a subject that is a source such as a memory store or a document rather than the actor who did it.",
    "STEP 3 if such words exist, reject with the matching code and copy exactly those words into evidence. If the quote carries no such words and does support the item, pass with the words that support it. If the quote is too short or genuinely two-way, answer uncertain, whose evidence may be null.",
    "STEP 4 never repair a row, never add an item, never answer for an item that is not in this call. One verdict per item, and every pass or reject carries one contiguous run of the quote as evidence.",
];

const STANDARDS_FACT_REVIEW_CHALLENGE: &[&str] = &[
    "ONLY THE QUOTE IS EVIDENCE: judge the characters of the quote. The target turn and the CTX beside it are context; a doubt that lives only in the context is not counter-evidence.",
    "ADVERSARIAL, NOT CREATIVE: you look for counter-evidence that is already written, not for a reason the item might be wrong. No counter-evidence in the quote means pass, not uncertain.",
    "NO REPAIR, NO NEW ROWS: never rewrite a quote, a value or a subject, never merge two items, never add an item that is not in this call. Your only output is one verdict per item.",
    "CODES ARE CLOSED: quote_mismatch when the quote does not carry the subject, predicate, value or category; missing_subject only when neither the quote nor the `speaker:` line of that TARGET TURN identifies an actor; wrong_subject when the subject is a source such as a memory store, a project memory, a stage ledger, a log or a document instead of the person, organization or agent that did it; not_durable when the quote shows it is not a durable commitment; other for anything else. pass always carries code null; uncertain may carry null or one of these codes.",
    "EVIDENCE IS THE SAME CONTRACT: pass and reject both carry one contiguous run of characters copied from that item's own quote; never a summary and never a paraphrase. A verdict without it, or with evidence outside the quote, is read as uncertain.",
    "DURABILITY IS COUNTED THE SAME WAY: a plan for the next few days and a recurring schedule still hold after the conversation ends, so they are not counter-evidence; an in-session work step, a greeting or a one-off request is.",
    // R2-1：与 proposer 同一条判据（两档判据一致性，工单 §7.2 的延伸）。
    ABSENT_NEIGHBORS_NOTE,
];

const POSITIVES_FACT_REVIEW_CHALLENGE: &[&str] = &[
    "GOOD (no counter-evidence -> pass): ITEM f1 quote \"我下周五之前把对账单发给你\" -> searching the quote for a withdrawal, a condition, a different object or a non-actor subject finds none; the words 我下周五之前 name the actor and the time, so the item stands: {\"ref\":\"f1\",\"verdict\":\"pass\",\"code\":null,\"evidence\":\"我下周五之前\"}",
];

const NEGATIVES_FACT_REVIEW_CHALLENGE: &[&str] = &[
    "BAD (reject without counter-evidence): ITEM f2 quote \"我下周五之前把对账单发给你\" is rejected only because the target turn sounds casual. The quote itself carries the actor, the commitment and the time, so the honest answer is pass with evidence \"把对账单发给你\".",
    "BAD (uncertain when the quote settles it): ITEM f3 quote \"好的，收到\" is answered uncertain. The quote contains nothing durable at all, so it is counter-evidence enough: reject with not_durable and evidence \"好的，收到\".",
    "BAD (repairing the item): a challenge never writes a corrected value or a new row. It answers the same four-key verdict for that ref and nothing else.",
    "BAD (evidence outside the quote): evidence \"李四\" when the item's quote is \"我下周五之前把对账单发给你\" is not inside the quote; a verdict whose evidence is not a contiguous run of the quote is read as uncertain.",
    "BAD (borrowing context): rejecting because the turn before the quote mentions a different deadline; the CTX and target turn are not evidence and that deadline is outside the quote.",
];

/// 反判的输出契约：与 proposer 同形（四键、code 闭集、`pass` 值同），只换"这一侧在干什么"。
const TAIL_FACT_REVIEW_CHALLENGE: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly four keys, all required: schema_version, pass, family, verdicts. schema_version is 0 and pass is \"fact_review\".
{\"schema_version\":0,\"pass\":\"fact_review\",\"family\":\"%FAMILY%\",\"verdicts\":[{\"ref\":\"f1\",\"verdict\":\"pass\",\"code\":null,\"evidence\":\"我下周五之前\"},{\"ref\":\"f2\",\"verdict\":\"reject\",\"code\":\"not_durable\",\"evidence\":\"好的，收到\"},{\"ref\":\"f3\",\"verdict\":\"reject\",\"code\":\"quote_mismatch\",\"evidence\":\"下周再聊\"}]}
This call: family=%FAMILY%. Every item below belongs to this family; any other family value is rejected.
verdicts[]: one entry per ITEM, {\"ref\",\"verdict\",\"code\",\"evidence\"}. ref is the ITEM f-ref printed for that row and appears exactly once. verdict is one of pass, reject, uncertain. code is null when verdict is pass, and uncertain may also carry null; otherwise code is one of quote_mismatch, not_durable, missing_subject, wrong_subject, other. reject means you found words in the quote that defeat the item; pass means you searched the whole quote and found no such words.
evidence is required for pass and reject and must be one contiguous run of characters copied from that item's own quote, never a summary, never a paraphrase, never text from the target turn or the CTX. uncertain may carry null. A pass or reject without evidence, or with evidence outside the quote, is read as uncertain.
Do not write any identity, turn reference, anchor reference, timestamp or millisecond value. Extra keys are rejected. Do not add items, do not drop items, do not rewrite a row. Text stays in the language of the source.";

/// challenger 的 system = 自己的 canary（首行一次）+ 六段，仍走同一个 `assemble`。
pub fn fact_review_challenge_system(family: FactFamily) -> String {
    let tail = TAIL_FACT_REVIEW_CHALLENGE.replace("%FAMILY%", family.slug());
    assemble(FACT_REVIEW_CHALLENGE_CANARY, HEAD_FACT_REVIEW_CHALLENGE, &tail,
             METHOD_FACT_REVIEW_CHALLENGE, STANDARDS_FACT_REVIEW_CHALLENGE,
             POSITIVES_FACT_REVIEW_CHALLENGE, NEGATIVES_FACT_REVIEW_CHALLENGE)
}

pub fn fact_review_challenge_shape() -> PromptShape {
    shape_of(METHOD_FACT_REVIEW_CHALLENGE, STANDARDS_FACT_REVIEW_CHALLENGE,
             POSITIVES_FACT_REVIEW_CHALLENGE, NEGATIVES_FACT_REVIEW_CHALLENGE)
}

// ── 审核输入的信息段（C6a）：只补信息，不改指令文案 ─────────────────────────
//
// 审核模型此前只见「行 + 引文 + 目标轮正文」，问它"谁做的/条件丢没丢"却看不到谁在说话。
// 这一层补四类**程序已有**的信息，四份模板的指令（HEAD/METHOD/STANDARDS/POSITIVES/
// NEGATIVES/TAIL）一个字不动：
//
// 1. 批头：`session_key` / `owner_agent_id` / `visibility` / `scope`（归属由调用方从
//    review item 的 meta 与载荷 owner 取，渲染层不查库）；
// 2. 每轮角色与说话人：`TARGET TURN` 标 `role`，说话人是**可读名**（agent ⇒
//    `source_agent`；human ⇒ 主人锚的 canonical，与 C1 的 `speaker_anchor_id` 同一套
//    解析，见 `review::run::review_context`），解析不出就是 `unknown`；
// 3. ±1 邻轮：目标轮的前一轮与后一轮只进 `CTX (not evidence)` 段，明说"仅上下文、
//    不得作为证据"；压缩摘要块也只允许出现在这一段；
// 4. 条件标记：程序扫目标轮正文里的条件词并在 TARGET 下写 `condition_markers:`，
//    **只提示、不判定**——是否真的丢了条件仍归模型。
//
// 纪律与既有渲染相同：`turn_uid` / `anchor_id` / 毫秒 / 生成理由一个字都不出现。

/// 审核输入的批级信息（C6a）。`speakers` 与 `batch.rows` **逐位对应**，
/// 由调用方用 C1 的解析算好（`review::run::review_context`），渲染层只按位置取。
#[derive(Debug, Clone, Default)]
pub struct ReviewInputContext {
    pub session_key: String,
    pub owner_agent_id: String,
    pub visibility: String,
    /// `scope_kind:scope_key`（例：`project:proj-x`）。
    pub scope: String,
    pub speakers: Vec<String>,
}

impl ReviewInputContext {
    /// 说话人名的兜底：身份未知时的可读字面量（与 C1 的"不造无名用户"同一取向）。
    pub const UNKNOWN_SPEAKER: &'static str = "unknown";

    /// 按位置取说话人名；`speakers` 比批短（调用方 bug）时写 `unknown`，不静默错位。
    fn speaker(&self, index: usize) -> &str {
        self.speakers
            .get(index)
            .map(String::as_str)
            .unwrap_or(Self::UNKNOWN_SPEAKER)
    }
}

/// 条件词的扫描顺序：**同一起点最长优先**（`之后` 命中就不再单独记 `后`）。
/// `如果/若/除非/确认后/之后/一旦/万一/要是/前/后` 是派发档给定的全表，顺序只影响
/// 同位置的重叠取舍，不改变"哪些词算条件标记"。
const CONDITION_WORDS: [&str; 10] =
    ["确认后", "如果", "之后", "除非", "一旦", "万一", "要是", "若", "前", "后"];

/// 目标轮正文里命中的条件词（按出现顺序、去重）。只做提示，不判定条件是否丢失。
pub fn condition_markers(body: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        let hit = CONDITION_WORDS.iter().copied().find(|word| rest.starts_with(word));
        match hit {
            Some(word) => {
                if !out.contains(&word) {
                    out.push(word);
                }
                rest = &rest[word.len()..];
            }
            None => {
                let mut chars = rest.chars();
                chars.next();
                rest = chars.as_str();
            }
        }
    }
    out
}

/// 批头：一行，四格都来自 review item（不查库）。
fn render_session_header(s: &mut String, ctx: &ReviewInputContext) {
    s.push_str(&format!(
        "SESSION session_key={} | owner_agent_id={} | visibility={} | scope={}\n",
        prepare(&ctx.session_key),
        prepare(&ctx.owner_agent_id),
        prepare(&ctx.visibility),
        prepare(&ctx.scope)
    ));
}

/// `TARGET TURN` 段头：角色与说话人放在同一行（说话人由程序给，模型不从正文猜）。
fn render_target_head(
    s: &mut String,
    row: &crate::batch::TurnRow,
    index: usize,
    ctx: &ReviewInputContext,
) {
    s.push_str(&format!(
        "TARGET TURN [role: {} | speaker: {}]:\n",
        prepare(&row.role),
        prepare(ctx.speaker(index))
    ));
}

/// ±1 邻轮：**只进 CTX 段**。没有邻轮时明写 `CTX: (none)`，不静默省略。
///
/// 压缩摘要块（[`is_compaction_body`]）允许出现在这里（它是上下文、不是证据），
/// 但标签上写明 `compact summary`，避免被读成某人说过的话。
fn render_ctx(s: &mut String, batch: &Batch, target_index: usize) {
    let mut wrote_header = false;
    let neighbors = [
        target_index.checked_sub(1).map(|index| ("prev", index)),
        (target_index + 1 < batch.rows.len()).then_some(("next", target_index + 1)),
    ];
    for (label, index) in neighbors.into_iter().flatten() {
        if !wrote_header {
            s.push_str("CTX (not evidence):\n");
            wrote_header = true;
        }
        let row = &batch.rows[index];
        let mark = if is_compaction_body(&row.body) { " [compact summary]" } else { "" };
        s.push_str(&format!("  {label}{mark}:\n"));
        s.push_str(&prepare(&row.body));
        s.push('\n');
    }
    if !wrote_header {
        s.push_str("CTX: (none)\n");
    }
}

/// user = `PASS fact_review v0 | family <slug>` 首行 + 归属批头 + 逐条 ITEM。
///
/// 每条只给：`ref f1`（按 `drafts` 位置编号，调用方构造 `parse_fact_review` 的 `refs` 时
/// 必须用同一套编号）、subject canonical、predicate、object/value、category、quote、
/// 目标轮（`role` + 说话人 + 正文 + 条件词提示）与 ±1 邻轮（只进 `CTX` 段）。
/// **不给** anchor_id / turn_uid / 毫秒 / confidence / 抽取侧写下的整句
/// （那是抽取的结论，不是证据）——审核一旦读到抽取的结论就变成自证。
///
/// 渲染前逐格过 `prepare`：`value` / `quote` 是从用户原话里摘的，可能带围栏块或零宽字符；
/// 审核输入与抽取输入走同一把剪刀（本文件头第 3 条纪律），不然审核面就是第二个注入入口。
pub fn fact_review_user(
    family: FactFamily,
    drafts: &[FactDraft],
    batch: &Batch,
    ctx: &ReviewInputContext,
) -> String {
    let mut s = String::from("PASS fact_review v0 | family ");
    s.push_str(family.slug());
    s.push('\n');
    render_session_header(&mut s, ctx);
    for (index, draft) in drafts.iter().enumerate() {
        // 编号由**位置**决定，与 `fact_user` 的 t/a 同一套纪律：调用方与 parser 不各读一份。
        s.push_str(&format!("ITEM f{}\n", index + 1));
        s.push_str(&format!("subject: {}\n", prepare(&draft.subject_canonical)));
        s.push_str(&format!("predicate: {}\n", prepare(&draft.predicate)));
        if let Some(object) = &draft.object_canonical {
            s.push_str(&format!("object: {}\n", prepare(object)));
        }
        if let Some(value) = &draft.value {
            s.push_str(&format!("value: {}\n", prepare(value)));
        }
        s.push_str(&format!("category: {}\n", prepare(&draft.category)));
        s.push_str(&format!("quote: {}\n", prepare(&draft.quote)));
        match batch.rows.iter().position(|row| row.turn_uid == draft.turn_uid) {
            Some(target) => {
                let row = &batch.rows[target];
                render_target_head(&mut s, row, target, ctx);
                s.push_str(&prepare(&row.body));
                s.push('\n');
                let markers = condition_markers(&row.body);
                if !markers.is_empty() {
                    s.push_str(&format!("condition_markers: {}\n", markers.join(", ")));
                }
                render_ctx(&mut s, batch, target);
            }
            // 目标轮不在批里是调用方的错（parse 阶段已拒），这里明说而不是悄悄少给一段。
            None => {
                s.push_str("TARGET TURN: (not in this batch)\n");
                s.push_str("CTX: (none)\n");
            }
        }
    }
    s
}

// ── 重跑（§7.3b）：只判一条 uncertain 草稿的第三次调用 ─────────────────────────
//
// 首审（proposer）与反判（challenger）都判不出来时，这条草稿不是"再问一遍同样的东西"
// ——那只会得到同一个答案。重跑要给**首审没有的上下文**：引文在同一轮里的前后原文、
// 整轮正文、前后邻轮、程序切出来的同轮子句清单、候选锚全表。它仍然只判一条、仍然只把
// quote 当证据，区别只在"读得懂"：谁在说话、"这个"指哪句、条件落在切口的哪一侧。
//
// 三条纪律：
// 1. **自己的 canary**（[`FACT_REVIEW_RETRY_CANARY`]），与 proposer/challenger/抽取三向缺席；
// 2. **单条契约**：一次调用只有一条判决，不是 `verdicts[]` 数组——重跑的输入就是一条草稿；
// 3. **判不了仍出 uncertain + note**：note 说"缺哪类信息"，程序只存不判分（落 artifact）。

/// **重跑**模板的 canary（§7.3b）。同一批 item 的第三次独立调用：只判一条首审判 uncertain
/// 的草稿，且比首审看到更多上下文。与 [`FACT_REVIEW_CANARY`] / [`FACT_REVIEW_CHALLENGE_CANARY`]
/// 必须都不同——同一串出现在两处，"这段输出是谁说的"就分不开。
pub const FACT_REVIEW_RETRY_CANARY: &str = "__FACT_REVIEW_RETRY_3a92__";

/// 重跑一次调用最多带出引文两侧各多少字符的原文（字符，不是字节）。
///
/// 只要"够看清切口"：太长会让重跑输入迅速逼近 [`crate::review::run::REVIEW_INPUT_MAX_BYTES`]，
/// 太短又看不出"这句话是从哪种句子里切出来的"。40 字覆盖一句正常口语。
pub const RETRY_BOUNDARY_CHARS: usize = 40;

const HEAD_FACT_REVIEW_RETRY: &str = "You are the retry review pass of a memory center. This call re-judges exactly ONE row that an earlier review could not settle. You did not write it, you never rewrite it, and you never answer for any other row. This time you are given what the first review never saw: the rest of the turn on both sides of the quote, the whole turn the quote was copied from, the turns before and after it, every clause the program cut out of that turn, and the full anchor table. Use all of it to read the quote correctly — but only the quote is evidence.\n";

const METHOD_FACT_REVIEW_RETRY: &[&str] = &[
    "STEP 1 read the single ITEM below: subject, predicate, object or value, category and quote. It is the only row this call judges.",
    "STEP 2 read the QUOTE BOUNDARY block: the run inside [[...]] is the quote, the `before` and `after` runs are the rest of the same turn. They show where the sentence was cut and what it continues into. They are context, not evidence.",
    "STEP 3 read the whole TARGET TURN, its CLAUSES, the CTX turns on both sides of it and the CANDIDATES table. They answer what the quote alone could not: who is speaking, what `这个` or `那件事` points at, whether a condition or a deadline sits outside the cut, and whether the subject is a real actor in the anchor table.",
    "STEP 4 answer with exactly one verdict: pass when the quote supports this subject, predicate, object or value and category and this is a durable commitment; reject when the quote carries words that defeat it; uncertain only when the quote plus this wider context still cannot settle it. For uncertain, note names the kind of information that is missing in one sentence. Never repair the row and never answer for another row.",
];

const STANDARDS_FACT_REVIEW_RETRY: &[&str] = &[
    "EXACTLY ONE ROW: one object, for the one ITEM printed. A list, a second verdict or an answer for a row that is not in this call is rejected.",
    "ONLY THE QUOTE IS EVIDENCE: pass and reject carry one contiguous run of characters copied from that item's own quote. The boundary text, the rest of the turn, the neighbouring turns, the clause list and the anchor table explain the quote; they are never cited and never quoted as evidence.",
    "THE WIDER CONTEXT IS FOR READING, NOT FOR RE-JUDGING: use it to resolve what the quote means, not to add a claim the quote does not make. If the quote still does not carry the subject, the object or the value, that is still a reject or an uncertain.",
    "UNCERTAIN NAMES THE GAP: when you answer uncertain, note holds one short sentence naming the missing kind of information — a missing anchor, an unresolved reference, a time the text never states. The program stores note verbatim and never scores it, so it is your only way to say what the queue should fetch next.",
    "CODES ARE CLOSED: quote_mismatch when the quote does not carry the subject, predicate, value or category; missing_subject only when neither the quote nor the `speaker:` line of that TARGET TURN identifies an actor; wrong_subject when the subject is a source such as a memory store, a project memory, a stage ledger, a log or a document instead of the person, organization or agent that did it; not_durable when it is not a durable commitment; other for anything else. pass always carries code null; uncertain may carry null or one of these codes.",
    "DURABILITY IS COUNTED THE SAME WAY: a plan for the next few days and a recurring schedule still hold after the conversation ends; an in-session work step, a greeting or a one-off request does not.",
];

const POSITIVES_FACT_REVIEW_RETRY: &[&str] = &[
    "GOOD (the wider context settles what the quote alone could not): ITEM f1 subject 张三, predicate promise, object 李四, value 把对账单发给你, quote \"我下周五之前把对账单发给你\". The first review answered uncertain because the year behind 下周五 was missing, and the turn before it reads 今天是2026年10月28日 — context, not evidence. The quote itself still names the actor, the action and the deadline, so the row holds: {\"verdict\":\"pass\",\"code\":null,\"evidence\":\"我下周五之前\",\"note\":null}",
];

const NEGATIVES_FACT_REVIEW_RETRY: &[&str] = &[
    "BAD (answering more than one row): this call judges one row. A reply with two verdicts, or with a verdicts array, is rejected; the row it was asked about gets no answer at all.",
    "BAD (citing the wider context): evidence \"今天是2026年10月28日\" is outside the quote \"我下周五之前把对账单发给你\". Evidence must be copied from the quote, so that answer is read as uncertain.",
    "BAD (repairing the row with the extra context): the text after the quote says 改成下周五, so the value is rewritten to 下周五. A retry never rewrites a value, a subject or a quote; it answers one verdict for the row as written.",
    "BAD (uncertain with an empty note): answering uncertain and leaving note null tells the queue nothing about what to fetch next. Name the missing kind of information in one sentence.",
    "BAD (uncertain when the quote settles it): the quote is \"好的，收到\" and the whole turn is a greeting. That is not a commitment, so the answer is reject with not_durable, not uncertain.",
];

/// 重跑的输出契约：**单条**四键（`verdict`/`code`/`evidence`/`note`），没有 `schema_version`/
/// `pass`/`family`——这一次调用没有"批"可标识，只有一条判决。
const TAIL_FACT_REVIEW_RETRY: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly four keys, all required: verdict, code, evidence, note.
{\"verdict\":\"pass\",\"code\":null,\"evidence\":\"我下周五之前\",\"note\":null}
This call re-judges exactly one row: the single ITEM below. A second verdict, a list, or any extra key is rejected.
verdict is one of pass, reject, uncertain. code is null when verdict is pass, and uncertain may also carry null; otherwise code is one of quote_mismatch, not_durable, missing_subject, wrong_subject, other. A subject that is a memory store, a project memory, a stage ledger, a log or a document is not the actor: reject it with wrong_subject.
evidence is required for pass and reject and must be one contiguous run of characters copied from that item's own quote — never from the boundary text, the rest of the turn, the neighbouring turns, the clause list or the anchor table. uncertain may carry null. A pass or reject without evidence, or with evidence outside the quote, is read as uncertain.
note is one short sentence naming the kind of information that is missing; it belongs on uncertain and may be null otherwise. The program stores note verbatim and never scores it.
Do not write any identity, turn reference, anchor reference, timestamp or millisecond value. Extra keys are rejected. Do not rewrite the row. Text stays in the language of the source.";

/// system = 自己的 canary（首行一次）+ 方法四步 + 标准六条 + 正例 1 + 反例 5 + 单条契约。
///
/// 与两份既有审核共用 `assemble`：重跑不是"另一套模板框架"，只是同一批 item 的第三次
/// 调用。它不带 family 参数——这一次调用没有批，family 只出现在 user 首行的 `family <slug>`。
pub fn fact_review_retry_system() -> String {
    assemble(
        FACT_REVIEW_RETRY_CANARY,
        HEAD_FACT_REVIEW_RETRY,
        TAIL_FACT_REVIEW_RETRY,
        METHOD_FACT_REVIEW_RETRY,
        STANDARDS_FACT_REVIEW_RETRY,
        POSITIVES_FACT_REVIEW_RETRY,
        NEGATIVES_FACT_REVIEW_RETRY,
    )
}

pub fn fact_review_retry_shape() -> PromptShape {
    shape_of(
        METHOD_FACT_REVIEW_RETRY,
        STANDARDS_FACT_REVIEW_RETRY,
        POSITIVES_FACT_REVIEW_RETRY,
        NEGATIVES_FACT_REVIEW_RETRY,
    )
}

/// 引文边界：同一轮里 quote 前后的原文（各至多 [`RETRY_BOUNDARY_CHARS`] 字），quote 带 `[[...]]`。
///
/// 只读渲染：`body` 已过 `prepare`，`quote` 是 parser 验过"落在该轮正文里"的原文。
/// quote 找不到（数据不一致）时明写，不悄悄少给一段——那会让重跑看起来"上下文就是空的"。
fn render_quote_boundary(s: &mut String, body: &str, quote: &str) {
    s.push_str(
        "QUOTE BOUNDARY (the [[...]] run is the quote; `before`/`after` are the rest of the same turn — context, not evidence):\n",
    );
    match body.find(quote) {
        Some(at) => {
            s.push_str(&format!(
                "  before: {}\n",
                tail_chars(&body[..at], RETRY_BOUNDARY_CHARS)
            ));
            s.push_str(&format!("  quote: [[{}]]\n", prepare(quote)));
            s.push_str(&format!(
                "  after: {}\n",
                head_chars(&body[at + quote.len()..], RETRY_BOUNDARY_CHARS)
            ));
        }
        None => {
            s.push_str("  before: (quote not found in this turn)\n");
            s.push_str(&format!("  quote: [[{}]]\n", prepare(quote)));
            s.push_str("  after: (quote not found in this turn)\n");
        }
    }
}

/// 前 `limit` 个字符（按字符边界切，不按字节）。
fn head_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

/// 后 `limit` 个字符（按字符边界切）。
fn tail_chars(text: &str, limit: usize) -> &str {
    if limit == 0 {
        return "";
    }
    match text.char_indices().rev().nth(limit - 1) {
        Some((index, _)) => &text[index..],
        None => text,
    }
}

/// 同一轮被切出的全部子句（`c*` 编号 + 原文）：与抽取/首审同一份 [`clause_index`]。
fn render_retry_clauses(s: &mut String, batch: &Batch, row: usize) {
    s.push_str("CLAUSES OF THIS TURN (program-cut c1..cN; context, not evidence):\n");
    let clauses: Vec<ClauseRef> = clause_index(batch)
        .into_iter()
        .filter(|clause| clause.row == row)
        .collect();
    if clauses.is_empty() {
        s.push_str("  (none)\n");
        return;
    }
    for clause in clauses {
        s.push_str(&format!("  [{}] {}\n", clause.id, clause.text));
    }
}

/// user = `PASS fact_review_retry v0 | family <slug>` 首行 + 归属批头 + 一条 ITEM（五项上下文）。
///
/// 派发档 §2.2 的五项输入依次是：① 该草稿的六格；② 引文边界（同一轮里 quote 前/后的原文，
/// quote 被标出来）；③ 该轮完整正文 + 前后各一轮；④ 同一轮被切出的全部子句；⑤ 候选锚全表
/// （`a* | kind | canonical`）。
/// **不给** anchor_id / turn_uid / 毫秒 / confidence（与两份既有审核同一把尺子）。
pub fn fact_review_retry_user(
    family: FactFamily,
    draft: &FactDraft,
    batch: &Batch,
    ctx: &ReviewInputContext,
    candidates: &[FactCandidate],
) -> String {
    let mut s = String::from("PASS fact_review_retry v0 | family ");
    s.push_str(family.slug());
    s.push('\n');
    render_session_header(&mut s, ctx);
    // ① 该草稿的六格。ref 恒为 `f1`：这一次调用只有一条，编号没有第二个位置可用。
    s.push_str("ITEM f1 (the only row in this call)\n");
    s.push_str(&format!("subject: {}\n", prepare(&draft.subject_canonical)));
    s.push_str(&format!("predicate: {}\n", prepare(&draft.predicate)));
    if let Some(object) = &draft.object_canonical {
        s.push_str(&format!("object: {}\n", prepare(object)));
    }
    if let Some(value) = &draft.value {
        s.push_str(&format!("value: {}\n", prepare(value)));
    }
    s.push_str(&format!("category: {}\n", prepare(&draft.category)));
    s.push_str(&format!("quote: {}\n", prepare(&draft.quote)));
    match batch
        .rows
        .iter()
        .position(|row| row.turn_uid == draft.turn_uid)
    {
        Some(target) => {
            let row = &batch.rows[target];
            let body = prepare(&row.body);
            // ② 引文边界（同一轮的原文切口）。
            render_quote_boundary(&mut s, &body, &draft.quote);
            // ③ 整轮正文 + 条件词提示 + ④ 同轮子句 + 前后各一轮。
            render_target_head(&mut s, row, target, ctx);
            s.push_str(&body);
            s.push('\n');
            let markers = condition_markers(&row.body);
            if !markers.is_empty() {
                s.push_str(&format!("condition_markers: {}\n", markers.join(", ")));
            }
            render_retry_clauses(&mut s, batch, target);
            render_ctx(&mut s, batch, target);
        }
        // 目标轮不在批里是数据不一致（replay 已挡过一次），明写而不是悄悄少给一段。
        None => {
            s.push_str("QUOTE BOUNDARY: (target turn not in this batch)\n");
            s.push_str("TARGET TURN: (not in this batch)\n");
            s.push_str("CLAUSES OF THIS TURN: (none)\n");
            s.push_str("CTX: (none)\n");
        }
    }
    // ⑤ 候选锚全表（只作"主语是不是真行动者"的参照，不是证据）。
    s.push_str("CANDIDATES (a* | kind | canonical; context, not evidence):\n");
    if candidates.is_empty() {
        s.push_str("(none in this batch)\n");
    }
    for candidate in candidates {
        s.push_str(&render_candidate_line(candidate, &candidate.reference));
    }
    s
}

//
// ── event（S2c-1）：与 fact 并列的第二个抽取 pass ──────────────────────────────
//
// event 只问一件事：**已经发生、并且留下了持久结果**的原子动作/状态变化/观察。
// 它与 fact 是两次独立调用：自己的 canary、自己的 `pass` 值、自己的输出契约；
// 监管它的审核也是第三次独立调用（`event_review_*`）。
// 三份 canary 互不引用——同一个串出现在两处，"这段输出是谁说的"就再也分不开。

/// event 抽取模板的 canary。首行一次，只在 `event_system` 里出现。
pub const EVENT_CANARY: &str = "__EVENT_CANARY_3c81__";

/// event 审核模板的 canary（独立调用，设计 §4「审核自证」的反面）。
pub const EVENT_REVIEW_CANARY: &str = "__EVENT_REVIEW_CANARY_b46f__";

/// 抽取契约的 `pass` 值。
pub const PASS_EVENT: &str = "event";

/// 审核契约的 `pass` 值。指向的是**哪一次调用**，所以与 `PASS_EVENT` 不同名。
pub const PASS_EVENT_REVIEW: &str = "event_review";

/// event 契约的版本号：与 fact 一样**不冒充 v4**（设计 §2）。
pub const EVENT_SCHEMA_VERSION: u32 = 0;

/// `fact.category` 里属于 event pass 的三值（设计 §2.3）。`other` 是写库层的残值兜底，
/// 不是这一 pass 的合法输出。
pub const EVENT_CATEGORIES: [&str; 3] = ["event", "state_change", "observation"];

/// 设计 §2.3 的 `result` 闭集。模型不许写"大概好了"这类自由文本。
pub const EVENT_RESULTS: [&str; 5] = ["done", "failed", "blocked", "ongoing", "unknown"];

/// v0 只收 `stated`。`reported`/`inferred` 与 fact 同一档，留 S5b。
pub const EVENT_MODE_STATED: &str = "stated";

/// event 审核 `code` 的闭集（设计 §4「code 闭集」）。`pass` 的 code 必须是 null。
///
/// S2e-6：`wrong_subject` 对 event 是"actor 不是做出这个动作的人/组织/agent"——与 fact 的
/// 同名词一格语义：`项目记忆`/`阶段台账`/文档是**读到这句话的地方**，不是行动者。
pub const EVENT_REVIEW_CODES: [&str; 6] = [
    "quote_mismatch", "not_atomic", "result_unsupported", "missing_actor", "wrong_subject",
    "other",
];

const HEAD_EVENT: &str = "You are the event pass of a memory center. This call asks one question only: what has already happened or already started here, and is worth remembering later? You emit events and nothing else: no plans, no promises, no moods, no relations and no causes.\n";

const METHOD_EVENT: &[&str] = &[
    "STEP 1 read every TARGET turn once and ask: did something already happen or already start there, and would a later reader need it?",
    "STEP 2 pick the actor from the CANDIDATES a-refs; one actor and one action per row, a compound sentence becomes several rows. Object is an a-ref only when the source names it, otherwise a literal goes to object_text.",
    "STEP 3 copy quote character-for-character (no ellipsis, at most 120 characters); text is one plain sentence whose numbers and dates all appear in quote. result is one closed word supported by quote.",
    "STEP 4 give every TARGET t-ref a home: an event whose turn or evidence names it, or empty_turns; never missing.",
];

const STANDARDS_EVENT: &[&str] = &[
    "ALREADY HAPPENED OR STARTED: an action is an event once it has begun, whether or not it has ended. result is an attribute, not a gate: done, failed, blocked, ongoing or unknown; when the quote shows the action but not the outcome, write result=unknown. An action that began and has not ended yet is an event with result=ongoing — this rule asks whether it started, not whether it ended. An announcement of what you are about to do (「现在做…」「先做…」「接下来…」, unfinished when the turn ends), a promise, plan, hypothetical or mood is not an event — nothing has happened yet. A completion report (「X 完成」「测试通过」「部署成功」) IS an event. When a turn reports a result and then announces the next step, keep every reported result and drop the announcement. A process step with no finding or lasting effect (「我看了一下日志」) is not worth a row; a check is worth one only when the same sentence also reports what it found.",
    "CATEGORY IS THE TRIGGER'S CLASS: trigger is the shortest verb phrase copied character for character from quote and must be a substring of quote. Read category from it: observation for a perception or reporting verb (see, read, check, find, confirm, say, report) when the thing perceived is not an action the actor performed — never write the perceived content as a second row; state_change when quote asserts that a property of an anchor went from one value to another (recover, become, stop, start, break), and object or object_text must name the thing whose state changed; event when the actor performed an action with a beginning and an end whose point is not a before/after comparison (deploy, submit, send, delete, pay). PRECEDENCE: perception wins over the other two, a state change over a plain action. When none of the three holds, the turn produces no row and goes to empty_turns.",
    "ONE ACTOR, ONE ACTION: one actor and one action per row; a compound sentence becomes several rows. action is a short verb phrase, not a copy of the whole sentence. EXCEPTION: counts or readings of one action (87 行 / 402 passed / 1 failed, 517 = 334 + 183) are ONE result row, never several actions — this rule constrains different actions only.",
    "VERBATIM QUOTE AND GROUNDED EVERYTHING: quote is a substring copied from the target turn, without ... or …, at most 120 characters. Every digit or date inside text must be found in quote. time_text is a slice of quote copied character for character: widen the quote to include it or write null; never borrow a time from another sentence or turn, never normalize it (昨天 stays 昨天).",
    "RESULT IS CLOSED AND OPTIONAL: done, failed, blocked, ongoing or unknown, verbatim from the output contract. When the quote carries a result word, the row must support it; when the quote shows only that the action happened or started, write unknown — a missing outcome is never a reason to drop the row. When a step is still running or the outcome is not in yet, write ongoing or unknown, never done. Never guess.",
    "ACTOR IS WHO THE QUOTE NAMES: actor is the a-ref of the person, organization or agent that quote names as the doer; use the turn speaker ([self]/[principal]) only when quote names no third party. Naming an implementer but writing actor a1 is wrong.",
];

const POSITIVES_EVENT: &[&str] = &[
    "GOOD: TARGET t2 says \"实施者把迁移脚本跑完，两条校验都过了。接着我把开关切到新路径。\" and a2=实施者 -> one row: category=event, actor=a2, action=run_migration, trigger=跑完, result=done, quote=\"实施者把迁移脚本跑完，两条校验都过了\"; the announcement is dropped, so the turn is not empty.",
    "GOOD: TARGET t7 says \"日志里报的是连接池耗尽。我把上限从 20 提到 50，复测通过。\" -> one row: category=state_change, action=raise_pool_limit, trigger=复测通过, result=done, quote=\"我把上限从 20 提到 50，复测通过\"; the diagnosis is not a row.",
    "GOOD: TARGET t9 says \"上一轮的改动没生效，我回滚了\" -> category=event, result=failed, trigger=回滚, quote=\"上一轮的改动没生效，我回滚了\"; the earlier row claiming it landed is contradicted.",
];

const NEGATIVES_EVENT: &[&str] = &[
    "BAD: 李四查了日志，发现是网关超时 is one observation, never two rows: the timeout is not an action the actor performed.",
    "BAD: 服务恢复了 is a state change, not an event: nobody acted with a beginning and an end.",
    "BAD: \"接着我把开关切到新路径，然后观察两天\" is only an announcement: empty_turns.",
    "BAD: \"迁移脚本已经跑完\" and \"接着我把开关切到新路径\" as two rows invent a second event; an announcement is never a row.",
    "BAD: \"日志里报的是连接池耗尽\" is a cause with no action: empty_turns.",
    "BAD: \"你先看一下那三个会话，再决定要不要改\" is an instruction: empty_turns.",
    "BAD: \"最近还有别的吗？\" is a question, and it asserts nothing: empty_turns.",
    "BAD: \"[Tool call cancelled by host]\" is runtime output, never a quote: empty_turns.",
    "BAD: \"工作区现在是干净的，HEAD 在 cafebabe\" is a reading, not a change: empty_turns.",
    "BAD: TARGET t6 says \"实施者交回了回滚脚本\" but writes actor a1; actor must be the a-ref of 实施者.",
];

const TAIL_EVENT: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly four keys, all required: schema_version, pass, events, empty_turns. schema_version is 0 and pass is \"event\".
{\"schema_version\":0,\"pass\":\"event\",\"events\":[{\"ref\":\"e1\",\"turn\":\"t4\",\"category\":\"state_change\",\"actor\":\"a1\",\"action\":\"restore_config\",\"trigger\":\"改回去\",\"object\":null,\"object_text\":\"配置文件\",\"participants\":[\"a1\"],\"place\":null,\"result\":\"done\",\"mode\":\"stated\",\"time_text\":\"昨晚\",\"quote\":\"昨晚我把配置改回去，服务就恢复了\",\"evidence\":[\"t4\"],\"text\":\"张三昨晚把配置改回旧版本，服务恢复。\"}],\"empty_turns\":[\"t2\"]}
events[]: {\"ref\",\"turn\",\"category\",\"actor\",\"action\",\"object\",\"object_text\",\"participants\",\"place\",\"result\",\"mode\",\"time_text\",\"quote\",\"evidence\",\"text\"}. ref is unique in this reply; turn is a TARGET t-ref; category is one of event, state_change, observation; actor is a CANDIDATE a-ref; trigger is the shortest verb phrase copied character for character from quote and must be a substring of quote; action is a short label, not a copy of the whole sentence; object is a CANDIDATE a-ref or null; object_text is a literal or null; at most one of object and object_text is non-null; participants is a non-empty list of CANDIDATE a-refs that contains actor; place is a CANDIDATE a-ref or null; result is one of done, failed, blocked, ongoing, unknown; mode is always \"stated\"; time_text is the time phrase copied character for character from quote (it must be a substring of quote), or null when quote names no time at all (never write later, soon or a guessed date, and never pull a time phrase in from outside quote); quote is the verbatim substring of that turn; evidence is a non-empty list of TARGET t-refs that contains turn.
empty_turns[]: TARGET t-refs with nothing that already happened, each at most once.
Do not write any id, uid, anchor_id, *_ms field, confidence, occurred_at, cause or relation field. Extra keys such as occurred_at_ms, cause, relation, confidence, evidence_turn_uid or turn_uid are rejected. Do not invent a t-ref or an a-ref. Text stays in the language of the source.";

/// system = canary（首行一次）+ 方法四步 + 标准六条 + 正例 3 + 反例 10 + 输出契约尾段。
/// 形状与 fact 共用 `assemble`：event 不是"另一套模板框架"，只是另一个 pass。
pub fn event_system() -> String {
    assemble(EVENT_CANARY, HEAD_EVENT, TAIL_EVENT, METHOD_EVENT, STANDARDS_EVENT,
             POSITIVES_EVENT, NEGATIVES_EVENT)
}

pub fn event_shape() -> PromptShape {
    shape_of(METHOD_EVENT, STANDARDS_EVENT, POSITIVES_EVENT, NEGATIVES_EVENT)
}

/// user = `PASS event v0` 首行 + CTX 段 + 逐轮 TARGET + 候选锚。
///
/// 与 [`fact_user`] 同一副骨架：CTX（C6b）位于全部 TARGET 之前、只作上下文；TARGET 编号是
/// `batch.rows` 顺序（`t1..tN`），`role` 与正文都先过 `prepare`；TARGET 段后面同样接一句
/// [`SPEAKER_NOTE`]；CANDIDATES 只渲染 `a* | kind | canonical`（自锚/主人锚行尾带
/// `[self]` / `[principal]`，与 fact 同一个渲染点）。`place` 也只能从这里取，
/// 因为模型没有别的入口能拿到锚。
pub fn event_user(batch: &Batch, candidates: &[FactCandidate], ctx: &ExtractCtx) -> String {
    let mut s = String::from("PASS event v0\n");
    render_extract_ctx(&mut s, batch, ctx);
    for (index, row) in batch.rows.iter().enumerate() {
        let role = prepare(&row.role);
        // D 子 agent 接线：这一轮在库里带了说话人锚就用它的 `a*`，否则逐字回落到 role 推导。
        let speaker = turn_speaker_ref(row.speaker_anchor_id.as_deref(), &row.role, candidates)
            .map(|reference| format!(" speaker={reference}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "TARGET [t{}] {} {}{}: {}\n",
            index + 1,
            render_target_ts(row.created_at_ms),
            role,
            speaker,
            prepare(&row.body)
        ));
    }
    s.push_str(SPEAKER_NOTE);
    s.push('\n');
    s.push_str(SPEAKER_LINE_NOTE);
    s.push('\n');
    s.push_str("CANDIDATES (actor/object/participants/place 只能引用这些 a*):\n");
    if candidates.is_empty() {
        s.push_str("(none in this batch)\n");
    }
    for (index, candidate) in candidates.iter().enumerate() {
        // 批内短 id 由**位置**决定，与 parser/审核同一条纪律（位置即契约）。
        let reference = format!("a{}", index + 1);
        debug_assert_eq!(candidate.reference, reference);
        s.push_str(&render_candidate_line(candidate, &reference));
    }
    s
}

// ── event 审核（S2c-1）：第三次调用 ───────────────────────────────────────────
//
// 唯一问题：这是不是一个原子动作、有持久结果、参与者/结果有引文支持，且 actor 是
// 真做出这个动作的那一方（S2e-6：来源/文档/记忆库不是行动者）。
// 只读「行 + 引文 + 目标轮」；不给抽取侧写下的整句（`text`），不给自己打分的理由。

const HEAD_EVENT_REVIEW: &str = "You are the review pass of a memory center. This is a second, independent call: you did not write these rows and you never rewrite them. For each row answer exactly one question: is it one atomic action that already happened or already started (its result may be unknown), do its participants have support in the quote, and is the actor the entity that actually did it rather than a memory store or a document? Alongside that verdict you answer one further question about the same row, on its own axis: 这一条会改变以后的回答吗？ Would recalling this row later change what someone answers or does — keep — or is it a work log that only records what was already done — demote.\n";

const METHOD_EVENT_REVIEW: &[&str] = &[
    "STEP 1 read one ITEM at a time. An item is a row someone else extracted: its actor, action, object or object_text, participants, place, category and result, its quote, and the target turn the quote was copied from.",
    "STEP 2 answer the first half: is this one atomic action that happened or started, rather than a process step with no finding or lasting effect, or two actions merged into one row?",
    "STEP 3 answer the second half: does the quote support the participants and — when the row claims done, failed or blocked — the result word, and did this already happen or start? A missing result is not a failure: unknown passes when the action itself is in the quote.",
    "STEP 4 give each item exactly one verdict: pass when both halves hold, reject when either fails, uncertain when the quote alone cannot settle it. You never repair a row and never extract a new one.",
    // G1 刀 B：价值轴的第二次判断（问句逐字，答案两格）。
    "STEP 5 answer the value axis for every item, as a question of its own: 这一条会改变以后的回答吗？ An event that changed what later work depends on (a decision landed, a failure was found, a state changed for good) keeps changing later answers — keep. A routine operational step (a command run, a status check, a restart, a deploy that only confirms the expected) changes no later answer — demote. Write value_axis keep or demote on every item, whatever its verdict: correctness and value are two axes.",
];

const STANDARDS_EVENT_REVIEW: &[&str] = &[
    "THE QUOTE IS THE WHOLE EVIDENCE: judge the characters of the quote, not what the speaker probably meant and not what a careful reader could add from context.",
    "ATOMIC ONLY: two actions in one row is rejected with not_atomic. A check or parse step that reports no finding is a process step and is rejected too; when the same sentence reports what it found (tests green, commit exists, build finished), the finding is the event and it may pass.",
    "RESULT, WHEN CLAIMED, MUST BE SUPPORTED: when the row claims done, failed or blocked, the quote must carry that outcome; if it does not, reject with result_unsupported. When the row claims unknown — or the quote shows the action started but not the outcome — the action itself is enough; never reject for a missing result. missing_actor 只在 quote 与目标轮的说话人行都无法指认行动者时才用 — the reviewer sees role and speaker for every target turn, so an agent's own completion report counts as naming the agent.",
    "CODES ARE CLOSED: quote_mismatch when the quote does not carry the action, object, participants or category; not_atomic; result_unsupported; missing_actor; wrong_subject when the actor is a source such as a memory store, a project memory, a stage ledger, a log or a document instead of the entity that did it; other. pass always carries code null; uncertain may carry null or one of these codes. Never repair a row, never add an item, never merge two items.",
    // 台账 §10.26 缺陷 4：自述可以指认 actor（P-fix），但不能替代结果证据。
    "SELF-REPORT NEEDS A CHECKABLE RESULT: an agent's own completion report may name the actor. Whatever result it claims must be in the quote; a quote that shows the action happened or started but not the outcome is result=unknown and may pass. 「已完成」 with no object, count or state change is not an atomic action — not_atomic.",
    // G1 刀 B：价值轴与对错轴各判各的（与 fact 审核同一条口径）。
    "VALUE AXIS IS ITS OWN AXIS: value_axis answers only 这一条会改变以后的回答吗？ Demote a row that only logs a routine step, even when its verdict is pass. Keep an event that changed what later work depends on, even when you reject it for a correctness reason. Never demote for quote_mismatch and never pass for value: the two are written separately and the program treats them separately.",
    // 邻居池/矛盾那条不在这份里：event 的复核只看"是否单一动作 / 结果有没有支撑 / actor 对不对"。
];

const POSITIVES_EVENT_REVIEW: &[&str] = &[
    "GOOD: ITEM e1: actor 张三, action restore_config, object_text 配置文件, participants 张三, category state_change, result done, quote \"我把配置改回去，服务就恢复了\" -> one action, the quote names the actor and the recovery, and it already happened; the recovery still changes later answers, so the value axis is keep: {\"ref\":\"e1\",\"verdict\":\"pass\",\"code\":null,\"value_axis\":\"keep\"}",
];

const NEGATIVES_EVENT_REVIEW: &[&str] = &[
    "BAD: ITEM e2 quotes \"我看了一下日志\": a process step with no finding or lasting effect is rejected: {\"ref\":\"e2\",\"verdict\":\"reject\",\"code\":\"not_atomic\"}",
    "BAD: ITEM e3 claims result done but its quote reads \"可能修好了吧\" and shows no outcome: {\"ref\":\"e3\",\"verdict\":\"reject\",\"code\":\"result_unsupported\"}",
    "BAD: ITEM e4 claims actor 李四, but its quote never names a person and the target turn's speaker line names 张三 — the quote does not carry the claimed actor, so this is quote_mismatch (missing_actor is only for when neither the quote nor the speaker line identifies any actor at all): {\"ref\":\"e4\",\"verdict\":\"reject\",\"code\":\"quote_mismatch\"}",
    "BAD: ITEM e5 claims result done and its quote is only \"已完成\": no object, no count and no state change — there is no atomic action to judge, so it is rejected even though the speaker is the actor: {\"ref\":\"e5\",\"verdict\":\"reject\",\"code\":\"not_atomic\"}",
    // G1 刀 B：价值轴反例——这一行**对**（单一动作、有结果）但**没价值**（只是例行一步）。
    "BAD (value axis): ITEM e6 quotes \"部署脚本已经跑完，环境就绪\" and the verdict is pass, but a routine deploy that only confirms the expected changes no later answer: value_axis is demote, written next to the verdict: {\"ref\":\"e6\",\"verdict\":\"pass\",\"code\":null,\"value_axis\":\"demote\"}",
];

const TAIL_EVENT_REVIEW: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly three keys, all required: schema_version, pass, verdicts. schema_version is 0 and pass is \"event_review\".
{\"schema_version\":0,\"pass\":\"event_review\",\"verdicts\":[{\"ref\":\"e1\",\"verdict\":\"pass\",\"code\":null,\"value_axis\":\"keep\"},{\"ref\":\"e2\",\"verdict\":\"reject\",\"code\":\"not_atomic\",\"value_axis\":\"demote\"},{\"ref\":\"e3\",\"verdict\":\"reject\",\"code\":\"result_unsupported\",\"value_axis\":\"keep\"}]}
verdicts[]: one entry per ITEM, {\"ref\",\"verdict\",\"code\",\"value_axis\"}. ref is the ITEM e-ref printed for that row and appears exactly once. verdict is one of pass, reject, uncertain. code is null when verdict is pass, and uncertain may also carry null when the quote alone cannot settle the question; otherwise code is one of quote_mismatch, not_atomic, result_unsupported, missing_actor, wrong_subject, other.
value_axis answers 这一条会改变以后的回答吗？ and is exactly one of keep or demote, written for every item whatever its verdict; any other value is rejected, and a missing value_axis is read as keep.
Do not write any identity field, turn reference, anchor reference, timestamp or millisecond value. Extra keys are rejected. Do not add items, do not drop items, do not rewrite a row. Text stays in the language of the source.";

/// system = 审核 canary（首行一次）+ 方法五步 + 标准六条 + 正例 1 + 反例 5 + 输出契约。
/// 第 5 步是 G1 刀 B 的价值轴（与对错轴各判各的）。
pub fn event_review_system() -> String {
    assemble(EVENT_REVIEW_CANARY, HEAD_EVENT_REVIEW, TAIL_EVENT_REVIEW, METHOD_EVENT_REVIEW,
             STANDARDS_EVENT_REVIEW, POSITIVES_EVENT_REVIEW, NEGATIVES_EVENT_REVIEW)
}

pub fn event_review_shape() -> PromptShape {
    shape_of(METHOD_EVENT_REVIEW, STANDARDS_EVENT_REVIEW, POSITIVES_EVENT_REVIEW,
             NEGATIVES_EVENT_REVIEW)
}

/// user = `PASS event_review v0` 首行 + 归属批头 + 逐条 ITEM。
///
/// 每条只给：ref、actor/participants 的 canonical 名字、action、object/object_text、
/// place、category、result、quote、目标轮（`role` + 说话人 + 正文 + 条件词提示）与
/// ±1 邻轮（只进 `CTX` 段）。**不给** anchor_id / turn_uid / 毫秒 /
/// 抽取侧写下的整句 `text`——审核一旦读到抽取的结论就变成自证。
/// 逐格过 `prepare`：这些字段都是从用户原话里摘的，与抽取侧走同一把剪刀。
pub fn event_review_user(
    drafts: &[EventDraft],
    batch: &Batch,
    ctx: &ReviewInputContext,
) -> String {
    let mut s = String::from("PASS event_review v0\n");
    render_session_header(&mut s, ctx);
    for (index, draft) in drafts.iter().enumerate() {
        // 编号由位置决定，与 `parse_event_review` 的 refs 同一套纪律。
        s.push_str(&format!("ITEM e{}\n", index + 1));
        s.push_str(&format!("actor: {}\n", prepare(&draft.actor_canonical)));
        s.push_str(&format!("action: {}\n", prepare(&draft.action)));
        // §7.5 切片一b：trigger 是 category 的出处，审核必须看得到原词才能核"类别对不对"。
        s.push_str(&format!("trigger: {}\n", prepare(&draft.trigger)));
        if let Some(object) = &draft.object_canonical {
            s.push_str(&format!("object: {}\n", prepare(object)));
        }
        if let Some(object_text) = &draft.object_text {
            s.push_str(&format!("object_text: {}\n", prepare(object_text)));
        }
        let participants = draft.participant_canonicals.join(", ");
        s.push_str(&format!("participants: {}\n", prepare(&participants)));
        if let Some(place) = &draft.place_canonical {
            s.push_str(&format!("place: {}\n", prepare(place)));
        }
        s.push_str(&format!("category: {}\n", prepare(&draft.category)));
        s.push_str(&format!("result: {}\n", prepare(&draft.result)));
        s.push_str(&format!("time_text: {}\n", prepare(&draft.time_text)));
        s.push_str(&format!("quote: {}\n", prepare(&draft.quote)));
        match batch.rows.iter().position(|row| row.turn_uid == draft.turn_uid) {
            Some(target) => {
                let row = &batch.rows[target];
                render_target_head(&mut s, row, target, ctx);
                s.push_str(&prepare(&row.body));
                s.push('\n');
                let markers = condition_markers(&row.body);
                if !markers.is_empty() {
                    s.push_str(&format!("condition_markers: {}\n", markers.join(", ")));
                }
                render_ctx(&mut s, batch, target);
            }
            // 目标轮不在批里是调用方的错（parse 阶段已拒），这里明说而不是悄悄少给一段。
            None => {
                s.push_str("TARGET TURN: (not in this batch)\n");
                s.push_str("CTX: (none)\n");
            }
        }
    }
    s
}

// ── naming 窄审（S3b-3a）：A1 批的第四次调用 ──────────────────────────────────
//
// naming 的审核问的不是"引文撑不撑得住一行"（A1 的行没有 quote），而是**名字本身**：
// 每个 ANCHOR 的名字（canonical、每个 alias、每个 mention surface）是否逐字出现在引用的轮里、
// 是否指向一个具体的东西、kind 是否是轮里说的那一种、同一批里一个名字有没有指两处。
//
// 这一份只产 prompt：入队、调模型、写判定由 runner 接（本轮不碰 runner/队列/引擎）。
// 它是**独立的一次调用**：自己的 canary、自己的 `pass` 值、自己的六码闭集；与 A1 抽取的
// canary 不同串——两次调用的输出不能互相冒充。渲染只给短引用（`t1..tN` / `a1..aN`）、
// 名字与轮正文：`turn_uid`、`anchor_id`、毫秒、`confidence`/`resolution`/`discourse_role`
// 一律不进 prompt（它们是记账，不是名字证据）。

/// naming 审核模板的 canary（与 A1 抽取的 canary 不同串：两次独立调用，输出不能互相冒充）。
pub const NAMING_REVIEW_CANARY: &str = "__NAMING_REVIEW_CANARY_7f3a__";

/// 审核契约的 `pass` 值（指向"哪一次调用"，与抽取的 pass 值不同名）。
pub const PASS_NAMING_REVIEW: &str = "naming_review";

/// 六码闭集（设计 §6）。pass 的 code 必须是 null。
pub const NAMING_REVIEW_CODES: [&str; 6] = [
    "not_verbatim", "pronoun", "generic", "kind_mismatch", "same_surface_diff", "alias_conflict",
];

const HEAD_NAMING_REVIEW: &str = "You are the naming review pass of a memory center. This call asks one question only: does each ANCHOR name one specific thing whose names the quoted turns actually contain? You judge anchors; you never rename, merge, split or invent one.\n";

const METHOD_NAMING_REVIEW: &[&str] = &[
    "STEP 1 read every ANCHOR block: its kind, canonical, aliases, every mention surface and the TURN each name points at.",
    "STEP 2 for each name printed for that anchor (canonical, every alias, every mention surface) look for it character-for-character in the TURN body: case, spacing, punctuation, translation and abbreviation must match exactly; a name you cannot re-read verbatim is not a name.",
    "STEP 3 ask whether that name picks out one specific thing: a pronoun or a bare role/label cannot; then check that kind is what the TURN says the thing is.",
    "STEP 4 compare names across ANCHOR blocks in this call: one surface cannot name two different things, and an alias cannot be another block's canonical.",
];

// H6：六码逐码有规则、码间有优先级、`reject` 与 `uncertain` 的边界写死。前四条钉"判什么"，
// 后三条钉"几个码同时成立时报哪个、什么时候才可以不判"——弱模型最常错的三处都在这三条里
// （surface 冲突一律写 `alias_conflict`、把 `not_verbatim` 写成 `generic`、把可判的失败写成
// `uncertain`）。顺序与 [`NAMING_REVIEW_CODES`] 无关：那张表是闭集，这里的顺序只是判决次序。
const STANDARDS_NAMING_REVIEW: &[&str] = &[
    "ONE VERDICT PER ANCHOR: the verdict covers the anchor together with its aliases and mentions; one bad name rejects the anchor.",
    "VERBATIM, NOT PARAPHRASE: a title you added, a translation, an abbreviation or a different script is not verbatim.",
    "SPECIFIC, NOT A LABEL: a full personal name is specific; 客户 / 老板 / 项目 / 系统 are labels, unless the turn binds the role to a name (客户李四).",
    "KIND FROM THE TURN: person, agent, org, place, object, animal, work, event, concept - if the turn makes the thing something else, that is kind_mismatch.",
    "ONE CODE, FIRST THAT APPLIES: if several rules fail, report the first in this order: pronoun, generic, not_verbatim, kind_mismatch, alias_conflict, same_surface_diff.",
    "REJECT VS UNCERTAIN: a name that fails a rule is reject with its code; use uncertain only when the TURN text itself cannot decide (truncated or missing), never for something this prompt already settles.",
    "CROSS-ANCHOR NAMES: an alias equal to another block's canonical is alias_conflict; one surface resolving to two different things in this call is same_surface_diff.",
];

const POSITIVES_NAMING_REVIEW: &[&str] = &[
    "GOOD: a1 kind=person canonical=张三 aliases=[老张] mentioned as 张三 in t1 -> {\"ref\":\"a1\",\"verdict\":\"pass\",\"code\":null}.",
];

// H6：六码逐码一条反例，每条都把**判决 JSON 逐字写出来**（与 fact/event 的审核反例同体例）。
// 三条是新的（`alias_conflict` 不再只说"不能"，`not_verbatim` 与 `kind_mismatch` 第一次有例），
// 后两条钉块间关系：`alias_conflict` 判**带别名的那一块**，`same_surface_diff` 判**两块都判**。
const NEGATIVES_NAMING_REVIEW: &[&str] = &[
    "BAD: a1 canonical=他 in a turn that only says 他下周来 -> pronoun; answer {\"ref\":\"a1\",\"verdict\":\"reject\",\"code\":\"pronoun\"}.",
    "BAD: a2 canonical=客户 when the turn never gives that customer a name -> generic; answer {\"ref\":\"a2\",\"verdict\":\"reject\",\"code\":\"generic\"}.",
    "BAD: a2 alias=李四 while a3 canonical=李四 -> alias_conflict; answer {\"ref\":\"a2\",\"verdict\":\"reject\",\"code\":\"alias_conflict\"}.",
    "BAD: a1 alias=老张 while no TURN contains 老张 -> not_verbatim; answer {\"ref\":\"a1\",\"verdict\":\"reject\",\"code\":\"not_verbatim\"}.",
    "BAD: a1 kind=org canonical=张三 while the TURN uses 张三 as a person -> kind_mismatch; answer {\"ref\":\"a1\",\"verdict\":\"reject\",\"code\":\"kind_mismatch\"}.",
    "BAD: a1 mention 小王 points at t1 and a2 mention 小王 points at t3 for two different people -> same_surface_diff; answer {\"ref\":\"a1\",\"verdict\":\"reject\",\"code\":\"same_surface_diff\"} and the same for a2.",
];

/// 输出契约尾段。**没有占位符**：这一次调用只问一件事，`pass` 值固定成常量的一格。
const TAIL_NAMING_REVIEW: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly three keys, all required: schema_version, pass, verdicts. schema_version is 0 and pass is \"naming_review\".
{\"schema_version\":0,\"pass\":\"naming_review\",\"verdicts\":[{\"ref\":\"a1\",\"verdict\":\"pass\",\"code\":null},{\"ref\":\"a2\",\"verdict\":\"reject\",\"code\":\"generic\"},{\"ref\":\"a3\",\"verdict\":\"reject\",\"code\":\"same_surface_diff\"}]}
verdicts[]: one entry per ANCHOR, {\"ref\",\"verdict\",\"code\"}. ref is the ANCHOR a-ref printed for that block and appears exactly once. verdict is one of pass, reject, uncertain. code is null when verdict is pass; otherwise code is one of not_verbatim, pronoun, generic, kind_mismatch, same_surface_diff, alias_conflict.
Do not write any identity, turn reference, anchor id, timestamp or millisecond value. Extra keys are rejected. Do not add anchors, do not drop anchors, do not rewrite a name. Names stay in the language of the source.";

/// system = canary（首行一次）+ 方法四步 + 标准七条 + 正例 1 + 反例 6 + 输出契约尾段。
/// H6：标准从四到七、反例从三到六——六码逐码有规则与判决样例，码间优先级与 `reject`/
/// `uncertain` 的边界各占一条标准。method 仍是四步（每步问什么没变，变的只是判定口径）。
pub fn naming_review_system() -> String {
    assemble(NAMING_REVIEW_CANARY, HEAD_NAMING_REVIEW, TAIL_NAMING_REVIEW,
             METHOD_NAMING_REVIEW, STANDARDS_NAMING_REVIEW, POSITIVES_NAMING_REVIEW,
             NEGATIVES_NAMING_REVIEW)
}

pub fn naming_review_shape() -> PromptShape {
    shape_of(METHOD_NAMING_REVIEW, STANDARDS_NAMING_REVIEW, POSITIVES_NAMING_REVIEW,
             NEGATIVES_NAMING_REVIEW)
}

/// user = `PASS naming_review v0` 首行 + 逐轮 TURNS + 逐锚 ANCHORS。
///
/// - `TURNS:` 按 `batch.rows` 顺序，每条 `t{n} | {role}:` + 换行 + `prepare(&body)`；
/// - `ANCHORS:` 按 `a1.anchors` 顺序，每条 `ANCHOR a{n}` + kind/canonical/aliases/evidence
///   + 属于它的 mention 行；锚与锚之间一个空行；
/// - `evidence` 把 `turn_uid` 换成批内 `t` 序号（逗号无空格）；批外 uid 不渲染序号，
///   整段写 `evidence: -`——"无法逐字核对"要看得出来，而不是编一个序号；
/// - `speaker` 只写批内锚的 a-ref（位置编号），`discourse_role`/`resolution`/`confidence`
///   一个字都不出现：它们是 A1 的记账，不是名字；
/// - 不渲染 `reality_status`/`stable`（A1 双跑的记账），所有文本格过 `prepare`。
pub fn naming_review_user(a1: &A1Batch, batch: &Batch) -> String {
    let mut s = String::from("PASS naming_review v0\n");
    s.push_str("TURNS:\n");
    for (index, row) in batch.rows.iter().enumerate() {
        s.push_str(&format!("t{} | {}:\n", index + 1, row.role));
        s.push_str(&prepare(&row.body));
        s.push('\n');
    }
    s.push_str("ANCHORS:\n");
    // 批内短引用的**唯一**含义：位置 + 1（与 `fact_user` 的 t/a 同一套纪律）。
    let turn_of = |uid: &str| batch.rows.iter().position(|row| row.turn_uid == uid);
    let anchor_of = |reference: &str| {
        a1.anchors.iter().position(|anchor| anchor.reference == reference)
    };
    for (index, anchor) in a1.anchors.iter().enumerate() {
        if index > 0 {
            s.push('\n');
        }
        s.push_str(&format!("ANCHOR a{}\n", index + 1));
        s.push_str(&format!("kind: {}\n", prepare(&anchor.kind)));
        s.push_str(&format!("canonical: {}\n", prepare(&anchor.canonical)));
        if anchor.aliases.is_empty() {
            s.push_str("aliases: -\n");
        } else {
            s.push_str(&format!("aliases: {}\n", prepare(&anchor.aliases.join(", "))));
        }
        // evidence：批外 uid 让整段失去序号（不渲染 uid，也不渲染半个表）。
        let evidence: Vec<String> = anchor
            .evidence
            .iter()
            .filter_map(|entry| turn_of(&entry.turn_uid))
            .map(|turn| format!("t{}", turn + 1))
            .collect();
        if evidence.is_empty() || evidence.len() != anchor.evidence.len() {
            s.push_str("evidence: -\n");
        } else {
            s.push_str(&format!("evidence: {}\n", evidence.join(",")));
        }
        for mention in a1.mentions.iter().filter(|m| m.anchor_ref == anchor.reference) {
            let turn = match turn_of(&mention.turn_uid) {
                Some(turn) => format!("t{}", turn + 1),
                // 批外轮没有序号；uid 也不许顶上来。
                None => "-".to_string(),
            };
            let speaker = mention
                .speaker_ref
                .as_deref()
                .and_then(anchor_of)
                .map(|position| format!("a{}", position + 1))
                .unwrap_or_else(|| "-".to_string());
            s.push_str(&format!(
                "  mention: {} | turn {} | speaker {}\n",
                prepare(&mention.surface),
                turn,
                speaker
            ));
        }
    }
    s
}

// ── rows 窄审（S3c-2）：`/v1/rows` 的第五次调用 ──────────────────────────────
//
// 这一份只产 prompt：入队、调模型、写判定由 S3c-3 接（本轮不碰队列/引擎/HTTP）。
// 它是**独立的一次调用**：自己的 canary、自己的 `pass` 值、自己的五码闭集；与 naming 窄审
// 的 canary 不同串——两次调用的输出不能互相冒充。
//
// 唯一问题：**这一行有没有被它引用的那一轮撑住**。拍板 C1 把证据定成"行 `text` 对证据轮
// 正文"，所以审核看到的是行自己写下的那句 `text` 与 `evidence_turn_uid` 那一轮的正文，
// 不是 quote（fact 行的 `quote` 不在这条路上）。`conflict` 是既有事实的异值（S3c-3 填），
// 它只作为"同一主语同一谓词的另一个值"出现，用来判 `contradiction`。
//
// 渲染只给行 text、主语名、类别、谓词、冲突值与证据轮正文：`turn_uid`、`anchor_id`、毫秒、
// `confidence`/`owner` 一律不进 prompt（它们是记账，不是证据）。所有文本格过 `prepare`。
//
// S3c-fix（复审 P1-b）：`predicate` 从"闸只看非空、审核看不见"改成渲染进 `predicate:` 行。
// 判据是"同一主语同一谓词的异值即矛盾"，谓词不进 prompt，模型只能凭"看起来冲突"下手；
// 同时它是"这一行到底断言了什么"的唯一形状（`text` 是自然语言，`predicate` 才是键）。
// `uncertain` 也不再是"无码可填"：标准要求填**这条轮子无法裁定的那条规则的码**，
// 与 parser 的"非 pass 必须带五码之一"对齐（复审 P1-d 的尾段）。

/// rows 窄审模板的 canary（与 fact/event/naming 的 canary 都不同串：独立调用，输出不能冒充）。
pub const ROWS_REVIEW_CANARY: &str = "__ROWS_REVIEW_CANARY_4d21__";

/// 审核契约的 `pass` 值（指向"哪一次调用"）。
pub const PASS_ROWS_REVIEW: &str = "rows_review";

/// 五码闭集（设计 §4）。顺序即**判决优先级**：几条规则同时不成立时报最前的那一个。
/// `pass` 的 code 必须是 null。值域的家是这里，parser 与 prompt 都从这一格取。
pub const ROWS_REVIEW_CODES: [&str; 5] = [
    "unsupported",
    "wrong_subject",
    "invented_number",
    "contradiction",
    "not_atomic",
];

/// 一条待审行的渲染视图。
///
/// `reference` 是程序按位置分配的 `r1..rN`（与 `f1..`/`e1..`/`a1..` 同一套纪律：位置即契约）；
/// `evidence_body` 是 `evidence_turn_uid` 那一轮的正文，由调用方按 C1 取好——渲染层不查库，
/// 也就不可能悄悄换一轮。
#[derive(Debug, Clone)]
pub struct RowReviewItem {
    pub reference: String,
    pub text: String,
    pub subject_canonical: Option<String>,
    pub category: String,
    /// 行自己的谓词（`fact.predicate`）：判 `contradiction` 的前提是"同一主语同一谓词"，
    /// 所以它必须进 prompt（S3c-fix，复审 P1-b）。
    pub predicate: String,
    pub evidence_body: String,
    /// 既有事实的异值（S3c-3 填）；`None` 时整行 `conflict:` 不渲染。
    pub conflict: Option<String>,
}

const HEAD_ROWS_REVIEW: &str = "You are the row support review pass of a memory center. This call asks one question only: is each uploaded memory row supported by the turn it cites, with the right subject and no invented content? You judge rows; you never rewrite one.\n";

const METHOD_ROWS_REVIEW: &[&str] = &[
    "STEP 1 read each ROW: its text, subject, category, predicate, the TURN it cites, and any CONFLICT value.",
    "STEP 2 for every number or date printed in the row text, look for it in the TURN; a number that is not there is invented, even when it looks plausible.",
    "STEP 3 ask whether the TURN actually states this row: the action, state or decision must be present, the row's subject must be the one the TURN is about, and the row must carry one claim only.",
    "STEP 4 when a CONFLICT value is printed, ask whether the row contradicts it; a different value for the same subject and predicate is a contradiction unless the TURN says the value changed.",
];

// 标准恰七条（派发档逐字给定）：`unsupported`/`wrong_subject`/`invented_number`/`not_atomic`
// 各有一条自己的规则，`contradiction` 的口径落在方法第 4 步（同一主语同一谓词的异值即矛盾，
// 除非那一轮说值变了）与第 4 条反例上；最后两条钉"几个码同时成立时报哪个、什么时候才可以
// 不判"——弱模型最常错的两处（把可判的失败写成 `uncertain`、码序随手写）都在这两条里。
const STANDARDS_ROWS_REVIEW: &[&str] = &[
    "ONE VERDICT PER ROW: the verdict covers this row alone; never merge two rows.",
    "SUPPORTED, NOT PLAUSIBLE: the TURN must contain the claim, not merely make it likely.",
    "SUBJECT FROM THE TURN: a row whose subject is a tool, a document, a log or the memory store is wrong_subject.",
    "NUMBERS ARE LOUD: every number and date in the row must appear in the TURN; none may be inferred or corrected.",
    "ATOMIC: one claim per row; two actions or two states bundled into one row is not_atomic.",
    "REJECT VS UNCERTAIN: a row that fails a rule is reject with its code; use uncertain only when the TURN text is truncated or missing so the question cannot be decided; when uncertain, use the code of the rule the TURN cannot settle.",
    "ONE CODE, FIRST THAT APPLIES: if several rules fail, report the first in this order: unsupported, wrong_subject, invented_number, contradiction, not_atomic.",
];

const POSITIVES_ROWS_REVIEW: &[&str] = &[
    "GOOD: r1 text \"发布推迟到 12 月 3 日\", subject 某项目, TURN says the release moved to 12 月 3 日 -> {\"ref\":\"r1\",\"verdict\":\"pass\",\"code\":null}.",
];

// 五码逐码一条反例，每条把判决 JSON 逐字写出来（与 fact/event/naming 的审核反例同体例）。
const NEGATIVES_ROWS_REVIEW: &[&str] = &[
    "BAD: r1 text \"发布已经上线\" while the TURN only says it will ship later -> unsupported; answer {\"ref\":\"r1\",\"verdict\":\"reject\",\"code\":\"unsupported\"}.",
    "BAD: r1 subject 日志 while the TURN is about 张三 -> wrong_subject; answer {\"ref\":\"r1\",\"verdict\":\"reject\",\"code\":\"wrong_subject\"}.",
    "BAD: r1 text \"12 月 5 日\" while the TURN says 12 月 3 日 -> invented_number; answer {\"ref\":\"r1\",\"verdict\":\"reject\",\"code\":\"invented_number\"}.",
    "BAD: r1 says a promise is done while CONFLICT says pending and the TURN does not say it changed -> contradiction; answer {\"ref\":\"r1\",\"verdict\":\"reject\",\"code\":\"contradiction\"}.",
    "BAD: r1 bundles \"发布完成\" and \"旧版本回滚\" in one row -> not_atomic; answer {\"ref\":\"r1\",\"verdict\":\"reject\",\"code\":\"not_atomic\"}.",
];

/// 输出契约尾段。**没有占位符**：这一次调用只问一件事，`pass` 值固定成常量的一格。
const TAIL_ROWS_REVIEW: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly three keys, all required: schema_version, pass, verdicts. schema_version is 0 and pass is \"rows_review\".
{\"schema_version\":0,\"pass\":\"rows_review\",\"verdicts\":[{\"ref\":\"r1\",\"verdict\":\"pass\",\"code\":null},{\"ref\":\"r2\",\"verdict\":\"reject\",\"code\":\"unsupported\"}]}
verdicts[]: one entry per ROW, {\"ref\",\"verdict\",\"code\"}. ref is the ROW r-ref printed for that row and appears exactly once. verdict is one of pass, reject, uncertain. code is null when verdict is pass; otherwise code is one of unsupported, wrong_subject, invented_number, contradiction, not_atomic.
Do not write any identity, turn reference, anchor id, timestamp or millisecond value. Extra keys are rejected. Do not add rows, do not drop rows, do not rewrite a row. Text stays in the language of the source.";

/// system = canary（首行一次）+ 方法四步 + 标准七条 + 正例 1 + 反例 5 + 输出契约尾段。
pub fn rows_review_system() -> String {
    assemble(ROWS_REVIEW_CANARY, HEAD_ROWS_REVIEW, TAIL_ROWS_REVIEW, METHOD_ROWS_REVIEW,
             STANDARDS_ROWS_REVIEW, POSITIVES_ROWS_REVIEW, NEGATIVES_ROWS_REVIEW)
}

pub fn rows_review_shape() -> PromptShape {
    shape_of(METHOD_ROWS_REVIEW, STANDARDS_ROWS_REVIEW, POSITIVES_ROWS_REVIEW,
             NEGATIVES_ROWS_REVIEW)
}

/// user = `PASS rows_review v0` 首行 + 逐行 ROW 块。
///
/// 每行只给：ref（位置编号）、subject canonical（无则 `-`）、category、predicate、行 text、
/// conflict（无则整行省略）、证据轮正文。**不给** turn_uid / anchor_id / 毫秒：证据轮的
/// 身份由调用方在取正文时用掉，模型没有抄它的入口。
///
/// 逐格过 `prepare`：`text` 与证据轮正文都是用户原话（或从原话摘出来的句子），可能带围栏块
/// 或零宽字符；审核输入与抽取输入走同一把剪刀，不然审核面就是第二个注入入口。
pub fn rows_review_user(items: &[RowReviewItem]) -> String {
    let mut s = String::from("PASS rows_review v0\n");
    for (index, item) in items.iter().enumerate() {
        // 编号由**位置**决定，与 parser 的 refs 同一套纪律：调用方与 parser 不各读一份。
        let reference = format!("r{}", index + 1);
        debug_assert_eq!(item.reference, reference);
        if index > 0 {
            s.push('\n');
        }
        s.push_str(&format!("ROW {reference}\n"));
        match &item.subject_canonical {
            Some(subject) => s.push_str(&format!("subject: {}\n", prepare(subject))),
            // 无主语不是"省略这一格"：明写 `-`，审核才不会把缺格读成"没问到"。
            None => s.push_str("subject: -\n"),
        }
        s.push_str(&format!("category: {}\n", prepare(&item.category)));
        s.push_str(&format!("predicate: {}\n", prepare(&item.predicate)));
        s.push_str(&format!("text: {}\n", prepare(&item.text)));
        if let Some(conflict) = &item.conflict {
            s.push_str(&format!("conflict: {}\n", prepare(conflict)));
        }
        s.push_str("TURN:\n");
        s.push_str(&prepare(&item.evidence_body));
        s.push('\n');
    }
    s
}

#[cfg(test)]
mod notice_tests {
    use super::is_runtime_notice;

    /// 真实语料里被抽成事件的那一条原文。
    const OBSERVED: &str = "Model response interrupted. Completed output is preserved; \
automatic continuation stopped.";

    #[test]
    fn the_observed_notice_is_recognised() {
        assert!(is_runtime_notice(OBSERVED));
        assert!(is_runtime_notice(&format!("  \n{OBSERVED}\n")));
    }

    #[test]
    fn ordinary_utterances_are_not_notices() {
        assert!(!is_runtime_notice("张三明天把报价单发给你"));
        assert!(!is_runtime_notice("I saw a model response interrupted once"));
    }

    #[test]
    fn a_long_body_that_starts_with_the_prefix_is_not_a_notice() {
        // 正文里引用通知（讨论它）不是通知本身。
        let long = format!("{OBSERVED} 这段提示是 Qoder 在续写中断时插入的，我们要把它过滤掉。{}",
                           "补语".repeat(80));
        assert!(long.chars().count() > 240);
        assert!(!is_runtime_notice(&long));
    }
}

// ── P2 状态 pass（方案 `design/2026-10-31-state-pass-p2.md` §3–§4）───────────────
//
// 一个 pass 只问一个问题：**这句话是否直接陈述了某个主体在某一维度上的当前状态**。
// 与 event 的分工钉在判据里：event 问"发生了什么"（有起止的动作），state 问"现在是什么"
// （带有效期的断言）。`fact.category='state_change'` 仍然是"变化"那一格，本片不合并不删除。
//
// 三条从探针里带出来的硬事实（台账 §10.112/§10.114），它们是本段判据的出处：
// 1. **主体必须泛化**：只认"我"的 prompt 在 DSH 陪伴轮出 0 条；主体放开到任何锚/角色/物品
//    之后同一批出 6 条、逐字 6/6，LCCC 40 轮出 21 条、逐字 21/21。
// 2. **值逐字取自引文 = `expressed` 的程序判据**：需要换个词才说得出来的值是推导，
//    由程序分岔进隔离，模型不声明置信度也不声明 mode。
// 3. **单次不稳定**（逐字 Jaccard 0.36）⇒ 聚合与闸在这里不是可选项。

/// 状态模板的 canary（与抽取/审核其余四份都不同串：两次独立调用不许互相冒充）。
pub const STATE_CANARY: &str = "__STATE_CANARY_2c58__";

/// 状态窄审模板的 canary。
pub const STATE_REVIEW_CANARY: &str = "__STATE_REVIEW_CANARY_6a93__";

/// 这一条 pass 的 `pass_id`（账本键与状态键都用它）。
pub const PASS_STATE: &str = "state";

/// 审核契约的 `pass` 值：指向**哪一次调用**，所以与 `PASS_STATE` 不同名。
pub const PASS_STATE_REVIEW: &str = "state_review";

/// 状态契约的版本号：与 fact/event 一样不冒充 v4。
pub const STATE_SCHEMA_VERSION: u32 = 0;

/// `state_assertion.dimension` 的九值闭集。值域的家是 `rc-store` 的 DDL CHECK，
/// 这一格与它逐字对拍（`rc-memory/tests/writers_state.rs::dimension_closed_set_is_the_nine_ddl_values`
/// 钉住），**prompt 必须对齐、不加新维度**。那句用例比的是 `rc_memory::STATE_DIMENSIONS`
/// 这个副本：本文件与它目前靠人眼对齐（无跨 crate 对拍断言）。
pub const STATE_DIMENSIONS: [&str; 9] = [
    "emotion", "mood", "sentiment", "affect_episode", "trust", "relationship", "health",
    "config", "progress",
];

/// 原文直陈。**模型面唯一允许写的值**（载荷闭集只认它）。
pub const STATE_MODE_EXPRESSED: &str = "expressed";

/// 程序降级值（C1）：主语不在承载值的那一句里，或自锚行的值句里没有第一人称 ⇒ 程序盖这一值。
/// 模型写不出它，盖戳也只有一处（`review::gate` 的主语轴判定，写库组装与它同一份实现）。
pub const STATE_MODE_INFERRED: &str = "inferred";

/// `state_assertion.mode` 的两值闭集（DDL 的 CHECK、写库层校验、载荷闭集共用这一对）。
pub const STATE_MODES: [&str; 2] = [STATE_MODE_EXPRESSED, STATE_MODE_INFERRED];

/// 状态窄审 `code` 的闭集。`pass` 的 code 必须是 null。
pub const STATE_REVIEW_CODES: [&str; 6] = [
    "quote_mismatch", "not_a_state", "wrong_subject", "dimension_mismatch", "value_drift",
    "other",
];

const HEAD_STATE: &str = "You are the state pass of a memory center. This call asks one question only: does this turn directly state what some subject's current condition is, on one named dimension? You emit present states and nothing else: no actions, no events, no plans, no promises, no relations, no causes.\n";

const METHOD_STATE: &[&str] = &[
    "STEP 1 read every TARGET turn and ask: does the speaker plainly say what something is like right now — a feeling, a mood, an attitude, a trust level, a relationship, a health condition, a configuration, or how far a piece of work has got?",
    "STEP 2 pick the subject from the CANDIDATES a-refs. The subject can be anyone or anything the turn names: the speaker, another person, an animal, a device, a project. Never pick a subject the quote does not carry.",
    "STEP 3 write one row per (subject, dimension). dimension is one closed word; value_after is copied character for character out of quote — if you would have to change a word to say it, do not write the row.",
    "STEP 4 give every TARGET t-ref a home: a row whose turn or evidence names it, or empty_turns. A turn that only asks, only announces, or only reports what happened goes to empty_turns.",
];

const STANDARDS_STATE: &[&str] = &[
    "PRESENT CONDITION, NOT A CHANGE: the row says what something is like now. A sentence about an action that happened or started (「我把配置改回去了」「测试跑完了」) belongs to the event pass, not here — write empty_turns. A change sentence that also plainly states the resulting condition (「回滚了，现在服务是好的」) may yield one row for that stated condition.",
    "DIMENSION IS A CLOSED WORD: emotion (a feeling named in the words: 焦虑、生气、开心), mood (the prevailing temper: 心情不好、状态差), sentiment (an attitude toward something: 反感、满意), trust (how much something is believed: 信不过), relationship (who stands where to whom: 冷战、和好、搭档), health (body or medical: 失眠、发烧、康复中), config (a setting or setup as a state: 还是旧版本、已经切到新路径), progress (how far a piece of work has got: 写到一半、卡在登录那块). affect_episode is one named stretch of feeling with a beginning (「这阵子一直很低落」). Pick the one dimension the sentence actually names; when two fit, pick the one the words carry and leave the other out.",
    "SUBJECT IS ANY NAMED ENTITY: the subject is whoever or whatever the quote says is in that condition — a person, the speaker, an animal, a device, a project. 「我家猫今天不吃东西」 has the cat as subject, not the speaker. If the quote names no subject and the speaker is not plainly the one described, write empty_turns.",
    "VALUE IS VERBATIM: value_after must be a substring of quote, copied character for character, with nothing added. This is what makes the row a direct statement: a value that needs rewording is an inference, and inferences are set aside by the program, not stored. Keep it short — the words the speaker used, not the whole sentence.",
    "VERBATIM QUOTE, NO COMPLETION NEEDED: quote is a substring of the target turn, without ... or …, at most 120 characters, and it carries both the subject and the condition.",
    "DECLARATIVE ONLY: a question (「你是不是生气了？」) states nothing about anyone's condition — empty_turns. An announcement of what the speaker is about to do (「现在我去跑测试」) states nothing either — empty_turns. A wish or a hypothetical (「我希望明天好一些」) is not a present condition — empty_turns.",
];

const POSITIVES_STATE: &[&str] = &[
    "GOOD: TARGET t2 says \"我这两天焦虑得很，晚上睡不着。\" and a1=张三 (the speaker) -> one row: subject=a1, dimension=emotion, value_after=\"焦虑得很\", quote=\"我这两天焦虑得很，晚上睡不着\". The sleepless half is a second row only with dimension=health and value_after=\"睡不着\": one row per (subject, dimension).",
    "GOOD: TARGET t5 says \"小海不喜欢别人碰他的键盘，谁动他跟谁急。\" and a3=小海 -> one row: subject=a3, dimension=sentiment, value_after=\"不喜欢\", quote=\"小海不喜欢别人碰他的键盘，谁动他跟谁急\".",
    "GOOD: TARGET t8 says \"我家猫这两天不吃东西，蔫得很。\" and a4=猫 -> two rows: subject=a4 dimension=health value_after=\"不吃东西\"; subject=a4 dimension=mood value_after=\"蔫得很\". The speaker is not the subject of either.",
    "GOOD: TARGET t9 says \"网关那边我信不过了，上次它骗过我。\" and a2=网关 -> one row: subject=a2, dimension=trust, value_after=\"信不过\", quote=\"网关那边我信不过了，上次它骗过我\".",
];

const NEGATIVES_STATE: &[&str] = &[
    "BAD: 「你是不是生气了？」 is a question — it states nobody's condition: empty_turns.",
    "BAD: 「现在我去跑测试」 is an announcement about an action: empty_turns for this pass.",
    "BAD: 「我把网关换成新的了」 reports a change: that is the event pass, empty_turns here.",
    "BAD: for 「我最近压力挺大的」 writing value_after=\"焦虑\" is a reworded inference — value_after must be words present in quote (「压力挺大」).",
    "BAD: for 「小海不喜欢别人碰他的键盘」 writing subject=a1 (the speaker) is wrong — the quote names 小海 as the one who feels it.",
    "BAD: dimension=\"feeling\" is not one of the nine closed words; use emotion.",
    "BAD: one row carrying both 心情 and 关系 for the same subject is two rows, not one with a joined value.",
];

const TAIL_STATE: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly four keys, all required: schema_version, pass, states, empty_turns. schema_version is 0 and pass is \"state\".
{\"schema_version\":0,\"pass\":\"state\",\"states\":[{\"ref\":\"s1\",\"turn\":\"t4\",\"subject\":\"a1\",\"dimension\":\"emotion\",\"value_after\":\"焦虑得很\",\"quote\":\"我这两天焦虑得很，晚上睡不着\",\"evidence\":[\"t4\"]}],\"empty_turns\":[\"t2\"]}
states[]: {\"ref\",\"turn\",\"subject\",\"dimension\",\"value_after\",\"quote\",\"evidence\"}. ref is unique in this reply; turn is a TARGET t-ref; subject is a CANDIDATE a-ref; dimension is exactly one of emotion, mood, sentiment, affect_episode, trust, relationship, health, config, progress; value_after is a character-for-character substring of quote; quote is the verbatim substring of that turn, at most 120 characters; evidence is a non-empty list of TARGET t-refs that contains turn.
empty_turns[]: TARGET t-refs that state nobody's present condition, each at most once.
Do not write any id, uid, anchor_id, *_ms field, mode, confidence, valid_from, superseded_by or cause field. Extra keys such as mode, confidence, valid_from_ms, subject_kind or id are rejected. Do not invent a t-ref or an a-ref. Text stays in the language of the source.";

/// system = canary（首行一次）+ 方法四步 + 标准六条 + 正例 4 + 反例 7 + 输出契约尾段。
pub fn state_system() -> String {
    assemble(STATE_CANARY, HEAD_STATE, TAIL_STATE, METHOD_STATE, STANDARDS_STATE,
             POSITIVES_STATE, NEGATIVES_STATE)
}

pub fn state_shape() -> PromptShape {
    shape_of(METHOD_STATE, STANDARDS_STATE, POSITIVES_STATE, NEGATIVES_STATE)
}

/// 某主体在某维度上**已经活着**的当前状态（方案 §3 的 input 第三格）。
///
/// 它只进 prompt，不进契约：模型看见现值才不会把同一件事再说一遍，也让"同维度换值"
/// 一眼可辨（那是冲突，不是新状态）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingState {
    pub subject_canonical: String,
    pub dimension: String,
    pub value: String,
}

/// user = `PASS state v0` 首行 + CTX 段 + 逐轮 TARGET + 候选锚 + 已有当前状态。
///
/// 与 [`event_user`] 同一副骨架（CTX 在 TARGET 之前、TARGET 编号是 `batch.rows` 顺序、
/// 候选只渲染 `a* | kind | canonical`），多出的只有 `CURRENT STATES` 一段。
pub fn state_user(
    batch: &Batch,
    candidates: &[FactCandidate],
    ctx: &ExtractCtx,
    existing: &[ExistingState],
) -> String {
    let mut s = String::from("PASS state v0\n");
    render_extract_ctx(&mut s, batch, ctx);
    for (index, row) in batch.rows.iter().enumerate() {
        let role = prepare(&row.role);
        let speaker = turn_speaker_ref(row.speaker_anchor_id.as_deref(), &row.role, candidates)
            .map(|reference| format!(" speaker={reference}"))
            .unwrap_or_default();
        s.push_str(&format!(
            "TARGET [t{}] {} {}{}: {}\n",
            index + 1,
            render_target_ts(row.created_at_ms),
            role,
            speaker,
            prepare(&row.body)
        ));
    }
    s.push_str(SPEAKER_NOTE);
    s.push('\n');
    s.push_str(SPEAKER_LINE_NOTE);
    s.push('\n');
    s.push_str("CANDIDATES (subject 只能引用这些 a*):\n");
    if candidates.is_empty() {
        s.push_str("(none in this batch)\n");
    }
    for (index, candidate) in candidates.iter().enumerate() {
        let reference = format!("a{}", index + 1);
        debug_assert_eq!(candidate.reference, reference);
        s.push_str(&render_candidate_line(candidate, &reference));
    }
    s.push_str("CURRENT STATES (库里已经活着的当前值；同一主体同一维度换值就是冲突):\n");
    if existing.is_empty() {
        s.push_str("(none)\n");
    }
    for state in existing {
        s.push_str(&format!(
            "- {} | {} | {}\n",
            prepare(&state.subject_canonical),
            prepare(&state.dimension),
            prepare(&state.value)
        ));
    }
    s
}

// ── 状态窄审：第二次独立调用 ────────────────────────────────────────────────────
//
// 唯一问题：这条行的**引文**是否直说了一个当前状态、主语是不是引文里那一方、维度对不对、
// 值是不是引文里的原词。审核不改写、不补行、不打分。

const HEAD_STATE_REVIEW: &str = "You are the review pass of a memory center. This is a second, independent call: you did not write these rows and you never rewrite them. For each row answer exactly one question: does the quote plainly state a present condition of the subject it names, on the dimension it picked, with the value copied out of the quote?\n";

const METHOD_STATE_REVIEW: &[&str] = &[
    "STEP 1 read one ITEM at a time: its subject, dimension, value, quote, and the target turn the quote was copied from.",
    "STEP 2 answer the first half: does the quote state a condition that holds now, rather than ask about it, wish for it, announce an action, or report that something happened?",
    "STEP 3 answer the second half: is the subject the one the quote describes, is the dimension the one the words name, and is value a run of words that appears inside quote unchanged?",
    "STEP 4 give each item exactly one verdict: pass when both halves hold, reject when either fails, uncertain when the quote alone cannot settle it. You never repair a row and never extract a new one.",
];

const STANDARDS_STATE_REVIEW: &[&str] = &[
    "THE QUOTE IS THE WHOLE EVIDENCE: judge the characters of the quote, not what the speaker probably meant and not what a careful reader could add from context.",
    "A STATE, NOT AN EVENT: a quote that reports a finished action (「我改完配置了」) or a step in progress is not a present condition — reject with not_a_state. A question, a wish or an announcement is not a statement either.",
    "SUBJECT IS WHO THE QUOTE DESCRIBES: wrong_subject when the condition belongs to someone or something else in the quote (the cat is 不吃东西, not the speaker), or when the quote carries no subject at all.",
    "DIMENSION AND VALUE: dimension_mismatch when the words name a different one of the closed dimensions (「他最近总失眠」 is health, not trust); value_drift when value is not a run of words inside quote — a reworded or generalized value is an inference, and an inference never passes here.",
    "CODES ARE CLOSED: quote_mismatch when the quote does not carry the claimed condition at all; not_a_state; wrong_subject; dimension_mismatch; value_drift; other. pass always carries code null; uncertain may carry null or one of these codes. Never repair a row, never add an item, never merge two items.",
];

const POSITIVES_STATE_REVIEW: &[&str] = &[
    "GOOD: ITEM s1: subject 张三, dimension emotion, value 焦虑得很, quote \"我这两天焦虑得很，晚上睡不着\" -> the speaker names the feeling in those words, the value is inside the quote: {\"ref\":\"s1\",\"verdict\":\"pass\",\"code\":null}",
];

const NEGATIVES_STATE_REVIEW: &[&str] = &[
    "BAD: ITEM s2 quotes \"你是不是生气了？\" — a question states nobody's condition: {\"ref\":\"s2\",\"verdict\":\"reject\",\"code\":\"not_a_state\"}",
    "BAD: ITEM s3 has value 焦虑 while its quote reads \"我最近压力挺大\" — the word 焦虑 is not in the quote: {\"ref\":\"s3\",\"verdict\":\"reject\",\"code\":\"value_drift\"}",
    "BAD: ITEM s4 has subject 张三 while its quote is \"我家猫这两天不吃东西\" — the cat is the one described: {\"ref\":\"s4\",\"verdict\":\"reject\",\"code\":\"wrong_subject\"}",
    "BAD: ITEM s5 has dimension trust while its quote is \"他最近总失眠\" — the words name a body condition: {\"ref\":\"s5\",\"verdict\":\"reject\",\"code\":\"dimension_mismatch\"}",
];

const TAIL_STATE_REVIEW: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly three keys, all required: schema_version, pass, verdicts. schema_version is 0 and pass is \"state_review\".
{\"schema_version\":0,\"pass\":\"state_review\",\"verdicts\":[{\"ref\":\"s1\",\"verdict\":\"pass\",\"code\":null},{\"ref\":\"s2\",\"verdict\":\"reject\",\"code\":\"not_a_state\"}]}
verdicts[]: one entry per ITEM, {\"ref\",\"verdict\",\"code\"}. ref is the ITEM s-ref printed for that row and appears exactly once. verdict is one of pass, reject, uncertain. code is null when verdict is pass, and uncertain may also carry null when the quote alone cannot settle the question; otherwise code is one of quote_mismatch, not_a_state, wrong_subject, dimension_mismatch, value_drift, other.
Do not write any identity field, turn reference, anchor reference, timestamp or millisecond value. Extra keys are rejected. Do not add items, do not drop items, do not rewrite a row. Text stays in the language of the source.";

/// system = 审核 canary + 方法四步 + 标准五条 + 正例 1 + 反例 4 + 输出契约。
pub fn state_review_system() -> String {
    assemble(STATE_REVIEW_CANARY, HEAD_STATE_REVIEW, TAIL_STATE_REVIEW, METHOD_STATE_REVIEW,
             STANDARDS_STATE_REVIEW, POSITIVES_STATE_REVIEW, NEGATIVES_STATE_REVIEW)
}

pub fn state_review_shape() -> PromptShape {
    shape_of(METHOD_STATE_REVIEW, STANDARDS_STATE_REVIEW, POSITIVES_STATE_REVIEW,
             NEGATIVES_STATE_REVIEW)
}

/// 审核 user = `PASS state_review v0` 首行 + 归属批头 + 逐条 ITEM。
///
/// 每条只给：ref、subject 的 canonical 名、dimension、value、quote、目标轮（role +
/// 说话人 + 正文 + 条件词提示）与 ±1 邻轮。**不给** anchor_id / turn_uid / 毫秒 /
/// mode / confidence——审核一旦读到程序判出的 mode 就变成自证。
pub fn state_review_user(
    drafts: &[crate::pass_state_contract::StateDraft],
    batch: &Batch,
    ctx: &ReviewInputContext,
) -> String {
    let mut s = String::from("PASS state_review v0\n");
    render_session_header(&mut s, ctx);
    for (index, draft) in drafts.iter().enumerate() {
        s.push_str(&format!("ITEM s{}\n", index + 1));
        s.push_str(&format!("subject: {}\n", prepare(&draft.subject_canonical)));
        s.push_str(&format!("dimension: {}\n", prepare(&draft.dimension)));
        s.push_str(&format!("value: {}\n", prepare(&draft.value_after)));
        s.push_str(&format!("quote: {}\n", prepare(&draft.quote)));
        match batch.rows.iter().position(|row| row.turn_uid == draft.turn_uid) {
            Some(target) => {
                let row = &batch.rows[target];
                render_target_head(&mut s, row, target, ctx);
                s.push_str(&prepare(&row.body));
                s.push('\n');
                let markers = condition_markers(&row.body);
                if !markers.is_empty() {
                    s.push_str(&format!("condition_markers: {}\n", markers.join(", ")));
                }
                render_ctx(&mut s, batch, target);
            }
            None => {
                s.push_str("TARGET TURN: (not in this batch)\n");
                s.push_str("CTX: (none)\n");
            }
        }
    }
    s
}

// ── 波三刀 A：lesson（第 6 pass，从"记错"起步）───────────────────────────────
//
// 设计真源：`design/2026-11-08-g1-reuse-lesson-design.md` §波次三。这一支一次只问一个
// 语义任务：**这一段里犯了什么错、下次怎么做**。输出是 ReasoningBank 形状的三元组
// （title/description/content）+ 一枚错误轴 `error_kind`；它不抽事实、不抽事件、不改写
// 这一段，也不总结"这次做了什么"。程序侧的错因（`ERROR FAMILY`）由选批给，模型只负责
// 把"错在哪"说成一条可复用的规则——错的识别是程序的，规则化是模型的，两件事各归各的。
//
// 例子纪律与 fact/event 同一条：只用通用名与自编句，**不抄语料原句**。

/// lesson 抽取模板的 canary（与其余六份都不同串：独立调用不许互相冒充）。
pub const LESSON_CANARY: &str = "__LESSON_CANARY_7d3a__";

/// lesson 审核模板的 canary。
pub const LESSON_REVIEW_CANARY: &str = "__LESSON_REVIEW_CANARY_e15c__";

/// 抽取契约的 `pass` 值（与 `pass_lesson_contract::PASS_LESSON` 同一个词）。
pub const PASS_LESSON: &str = "lesson";

/// 审核契约的 `pass` 值：指向**哪一次调用**，所以与 [`PASS_LESSON`] 不同名。
pub const PASS_LESSON_REVIEW: &str = "lesson_review";

/// `ERROR FAMILY` 那一行的说明：它是**程序**在这一段里已经抓到的失败轴，不是证据本身。
/// 这一行进 user（抽取与审核各一次），契约里没有它的一格——模型写不回来。
pub const LESSON_PROGRAM_ERROR_NOTE: &str = "ERROR FAMILY is the program finding for this segment: an extraction pass ran here and its output was rejected. It names the axis of the failure, not its content, so read the TARGET turns to see what actually went wrong.";

const HEAD_LESSON: &str = "You are the lesson pass of a memory center. This call asks exactly one question about one short segment: what mistake was made here, and what should be done differently next time? You emit reusable rules and nothing else: no events, no facts, no work log, no summary of the conversation, no encouragement.\n";

const METHOD_LESSON: &[&str] = &[
    "STEP 1 read the ERROR FAMILY line and every TARGET turn. The family says which kind of failure the program already caught in this segment; the turns say what was actually said or done.",
    "STEP 2 name the concrete mistake: which claim, number, quote, shape or judgement went wrong, and what in the turns shows it. If the turns show no mistake a later reader could repeat, return an empty lessons array.",
    "STEP 3 write the rule for next time: title is a short imperative rule; description says when it applies and what it prevents; content says the concrete check or step to take before producing the same output. Write for a reader who never saw this session.",
    "STEP 4 put the failure on one axis: grounding (a number, date or fact with no source), quoting (text passed off as verbatim that is not), shape (output that cannot be read), judgement (right format, wrong call), other. At most three lessons, and split only when two mistakes have a different error_kind and a different fix.",
];

const STANDARDS_LESSON: &[&str] = &[
    "ONE SEGMENT, ONE QUESTION: the output is a rule, not a report of what happened. A sentence that only retells this segment is not a lesson.",
    "GENERALIZE THE NAMES: never copy a person name, a project name, a file path or a private identifier from the turns into a lesson, because those make the rule unusable in another session. Do not copy a whole source sentence either.",
    "ACTIONABLE OR NOTHING: content must tell the next reader what to check, count, copy or ask before producing the output. A general reminder to be careful is not a rule.",
    "TITLE IS THE HOOK: title states the rule in one short line (at most 60 characters), not the topic. A topic names the subject; a rule states the action.",
    "LENGTH IS A LIMIT: description and content are each at most 400 characters. Anything longer is a report wearing a rule clothes.",
    "EMPTY IS A LEGAL ANSWER: when the segment shows no repeatable mistake, write lessons as an empty array. Never invent a lesson to fill the reply.",
    "ERROR_KIND IS THE AXIS, NOT THE MESSAGE: write exactly one of grounding, quoting, shape, judgement, other.",
];

const POSITIVES_LESSON: &[&str] = &[
    "GOOD: ERROR FAMILY grounding; the turns report a count that appears nowhere in the source -> title \"引用数字前先回到原文\", description \"输出里要出现数量或日期时适用；防止把记忆里的估算写成语料原文\", content \"写任何数字前先在引文里找到同一个数字；找不到就写 unknown，并说明原文没有给数\", error_kind grounding.",
    "GOOD: ERROR FAMILY quoting; a row claims a sentence is verbatim but the characters were rewritten -> title \"引文逐字抄，不改写\", description \"引文会与原文逐字比对时适用；防止改写被当成原文\", content \"复制原文连续的一段，不改字、不加省略号；需要解释就在正文里另写一句\", error_kind quoting.",
];

const NEGATIVES_LESSON: &[&str] = &[
    "BAD: a paragraph retelling the session is a report, not a rule.",
    "BAD: \"下次让张三先跑测试\" names this session people, so it is unusable elsewhere; generalize to \"在下结论前先跑一遍测试\".",
    "BAD: \"要小心\" states no action, so it is not a lesson.",
    "BAD: splitting one mistake into three titles is three lessons for one fix; write one.",
    "BAD: error_kind \"quality\" is outside the closed set; judgement is the axis for a wrong call.",
];

const TAIL_LESSON: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly three keys, all required: schema_version, pass, lessons. schema_version is 0 and pass is \"lesson\".
{\"schema_version\":0,\"pass\":\"lesson\",\"lessons\":[{\"title\":\"引用数字前先回到原文\",\"description\":\"输出里要出现数量或日期时适用；防止把记忆里的估算写成语料原文\",\"content\":\"写任何数字前先在引文里找到同一个数字；找不到就写 unknown，并说明原文没有给数\",\"error_kind\":\"grounding\"}]}
lessons[]: {\"title\",\"description\",\"content\",\"error_kind\"}. title is the rule in one line, at most 60 characters; description says when the rule applies and what it prevents, at most 400 characters; content says the concrete check or step, at most 400 characters; error_kind is exactly one of grounding, quoting, shape, judgement, other. At most three lessons per reply, and lessons may be an empty array.
Do not write any id, uid, turn, ref, anchor, confidence, score, timestamp or millisecond field. Extra keys are rejected, including ref, turn_uid, id, evidence and confidence. Text stays in the language of the source.";

/// system = canary（首行一次）+ 方法四步 + 标准七条 + 正例 2 + 反例 5 + 输出契约尾段。
pub fn lesson_system() -> String {
    assemble(LESSON_CANARY, HEAD_LESSON, TAIL_LESSON, METHOD_LESSON, STANDARDS_LESSON,
             POSITIVES_LESSON, NEGATIVES_LESSON)
}

pub fn lesson_shape() -> PromptShape {
    shape_of(METHOD_LESSON, STANDARDS_LESSON, POSITIVES_LESSON, NEGATIVES_LESSON)
}

/// user = `PASS lesson v0` 首行 + 程序错因（`ERROR FAMILY`）+ 逐轮 TARGET。
///
/// 只给程序已经确定的两样东西：这一段属于哪个错误族（选批给的码）、这一段有哪几轮。
/// 不给锚表（lesson 不写锚）、不给抽取侧的结论（那是被拒的那一份，它正是"错"）。
pub fn lesson_user(batch: &Batch, error_code: &str) -> String {
    let mut s = String::from("PASS lesson v0\n");
    s.push_str(&format!("ERROR FAMILY: {}\n", prepare(error_code)));
    s.push_str(LESSON_PROGRAM_ERROR_NOTE);
    s.push('\n');
    for (index, row) in batch.rows.iter().enumerate() {
        s.push_str(&format!(
            "TARGET [t{}] {} {}: {}\n",
            index + 1,
            render_target_ts(row.created_at_ms),
            prepare(&row.role),
            prepare(&row.body)
        ));
    }
    s
}

// ── lesson 窄审：第二次独立调用 ────────────────────────────────────────────────
//
// 唯一问题：这条规则写的是不是这一段**真实发生**的错、轴对不对、下次的做法说不说得清。
// 只读「规则 + 这一段的目标轮 + 程序错因」；不给抽取模型的原始回复，也不给它自评的分数。
// EDV default-reject 在这一层兑现：没有 pass 就没有卡（引擎只写 pass 的草稿）。

const HEAD_LESSON_REVIEW: &str = "You are the review pass of a memory center. This is a second, independent call: you did not write these lessons and you never rewrite them. For each lesson answer exactly one question: does this rule describe a mistake that actually happened in the target turns, is its error_kind the right axis, and does it tell the next reader what to do?\n";

const METHOD_LESSON_REVIEW: &[&str] = &[
    "STEP 1 read one ITEM at a time: its title, description, content and error_kind, together with the ERROR FAMILY line and the TARGET turns that follow.",
    "STEP 2 check grounding: do the target turns show the mistake this rule claims? A rule that invents a failure the turns do not show is ungrounded.",
    "STEP 3 check the axis: does error_kind name the failure actually shown? A quoting failure labelled grounding is wrong_error.",
    "STEP 4 check the action: does content name a concrete check or step a later reader can take? Does the rule only retell this segment? A rule that cannot be used elsewhere, because it carries this session names, projects or paths, is not_actionable.",
    "STEP 5 give each item exactly one verdict: pass when the mistake is shown, the axis matches and the action is usable; reject when any of the three fails; uncertain when the turns are too thin to settle it. You never repair a lesson and never add one.",
];

const STANDARDS_LESSON_REVIEW: &[&str] = &[
    "THE TARGET TURNS ARE THE EVIDENCE: judge the turns, not what the extractor probably meant. The ERROR FAMILY line is a hint about the axis; it is not proof that the rule content is right.",
    "A RULE MUST STAND OUTSIDE THIS SESSION: a rule that only makes sense with this session names, project or file paths cannot be reused, so reject it with not_actionable. A rule that merely recounts the segment is restates.",
    "PASS IS NARROW: an empty lessons array is the extractor call and never reaches this call; you never add an item, never merge two items, never rewrite a row.",
    "CODES ARE CLOSED: ungrounded, wrong_error, not_actionable, restates, other. pass always carries code null; uncertain may carry null or one of these codes; reject must carry one of them.",
];

const POSITIVES_LESSON_REVIEW: &[&str] = &[
    "GOOD: ITEM l1: title \"引用数字前先回到原文\", description \"输出里要出现数量或日期时适用；防止把记忆里的估算写成语料原文\", content \"写任何数字前先在引文里找到同一个数字；找不到就写 unknown\", error_kind grounding, and the target turn really reports a count that the source never gives -> {\"ref\":\"l1\",\"verdict\":\"pass\",\"code\":null}",
];

const NEGATIVES_LESSON_REVIEW: &[&str] = &[
    "BAD: ITEM l2 claims the segment quoted loosely, but every target turn is a plain statement and nothing claims to be verbatim — the rule invents the failure: {\"ref\":\"l2\",\"verdict\":\"reject\",\"code\":\"ungrounded\"}",
    "BAD: ITEM l3 describes a misquoted sentence but writes error_kind grounding — the axis does not match what the turns show: {\"ref\":\"l3\",\"verdict\":\"reject\",\"code\":\"wrong_error\"}",
    "BAD: ITEM l4 content is a retelling of the segment with no rule for next time: {\"ref\":\"l4\",\"verdict\":\"reject\",\"code\":\"restates\"}",
];

const TAIL_LESSON_REVIEW: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence.
Exactly three keys, all required: schema_version, pass, verdicts. schema_version is 0 and pass is \"lesson_review\".
{\"schema_version\":0,\"pass\":\"lesson_review\",\"verdicts\":[{\"ref\":\"l1\",\"verdict\":\"pass\",\"code\":null},{\"ref\":\"l2\",\"verdict\":\"reject\",\"code\":\"ungrounded\"}]}
verdicts[]: one entry per ITEM, {\"ref\",\"verdict\",\"code\"}. ref is the ITEM l-ref printed for that row and appears exactly once. verdict is one of pass, reject, uncertain. code is null when verdict is pass; uncertain may also carry null; reject must carry one of ungrounded, wrong_error, not_actionable, restates, other.
Do not write any identity field, turn reference, anchor reference, timestamp or millisecond value. Extra keys are rejected. Do not add items, do not drop items, do not rewrite a row. Text stays in the language of the source.";

pub fn lesson_review_system() -> String {
    assemble(LESSON_REVIEW_CANARY, HEAD_LESSON_REVIEW, TAIL_LESSON_REVIEW,
             METHOD_LESSON_REVIEW, STANDARDS_LESSON_REVIEW, POSITIVES_LESSON_REVIEW,
             NEGATIVES_LESSON_REVIEW)
}

pub fn lesson_review_shape() -> PromptShape {
    shape_of(METHOD_LESSON_REVIEW, STANDARDS_LESSON_REVIEW, POSITIVES_LESSON_REVIEW,
             NEGATIVES_LESSON_REVIEW)
}

/// user = `PASS lesson_review v0` 首行 + 归属批头 + 程序错因 + 逐条 ITEM + 目标轮。
///
/// 目标轮整段只渲染一次（同一批所有 item 的证据是同一段），不按 item 复制：
/// 审核要判的是"规则和这一段对不对得上"，而这一段只有一份。
pub fn lesson_review_user(
    drafts: &[crate::pass_lesson_contract::LessonDraft],
    batch: &Batch,
    ctx: &ReviewInputContext,
    error_code: &str,
) -> String {
    let mut s = String::from("PASS lesson_review v0\n");
    render_session_header(&mut s, ctx);
    s.push_str(&format!("ERROR FAMILY: {}\n", prepare(error_code)));
    for (index, draft) in drafts.iter().enumerate() {
        s.push_str(&format!("ITEM l{}\n", index + 1));
        s.push_str(&format!("title: {}\n", prepare(&draft.title)));
        s.push_str(&format!("description: {}\n", prepare(&draft.description)));
        s.push_str(&format!("content: {}\n", prepare(&draft.content)));
        s.push_str(&format!("error_kind: {}\n", prepare(&draft.error_kind)));
    }
    s.push_str("TARGET TURNS (evidence for every item):\n");
    for (index, row) in batch.rows.iter().enumerate() {
        s.push_str(&format!(
            "TARGET [t{}] {} {}: {}\n",
            index + 1,
            render_target_ts(row.created_at_ms),
            prepare(&row.role),
            prepare(&row.body)
        ));
    }
    s
}
