//! 四作业各一份 system 模板，共用一个组装函数（守卫 3：形状只有一份）。
//! canary 串只住在这里，且只住字符串常量（坑 P-09）。

use crate::batch::{turn_ref, Batch};
use crate::prepare::prepare;

/// 交叉缺席用例（Q4）的三条 canary。判同与审核的在任务 7 / 任务 8 各自加。
pub const EXTRACT_CANARY: &str = "__EXTRACT_CANARY_6f3a__";
pub const JUDGE_CANARY: &str = "__JUDGE_CANARY_6f3a__";
pub const AUDIT_CANARY: &str = "__AUDIT_CANARY_6f3a__";
/// v4 A1（锚/提及）模板的 canary。四个作业各一个，仍只住这一个文件。
pub const ANCHOR_CANARY: &str = "__ANCHOR_CANARY_7c2e__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptShape {
    pub method_steps: usize,
    pub standards: usize,
    pub positives: usize,
    pub negatives: usize,
}

const METHOD: &[&str] = &[
    "STEP 1 read every turn below and list what is verifiable in it (a state, a decision, an event, a named entity).",
    "STEP 2 drop anything you cannot re-read from the turn itself; a guess is not a memory.",
    "STEP 3 write one item per fact, single-subject, short: title under 30 chars, body one sentence.",
    "STEP 4 attach the bracketed ref(s) you copied verbatim; if a turn carries nothing worth keeping, list its ref in worthless_turn_uids.",
];

const STANDARDS: &[&str] = &[
    "VERIFIABLE: the statement can be checked against the quoted turn, not against your prior knowledge.",
    "LONG-LIVED: it will still matter in a week; a passing mood is not an assertion unless it states a deadline.",
    "SINGLE: one item per fact. Do not merge two decisions into one body.",
    "SHORT: title is a label, body is one sentence. No bullets, no headings, no markdown fence.",
];

const POSITIVES: &[&str] = &[
    "GOOD: turn [t1] says \"we decided to ship behind a flag until the migration finishes\" -> {\"entities\":[],\"cards\":[],\"events\":[],\"assertions\":[{\"kind\":\"decision\",\"title\":\"ship behind a flag\",\"body\":\"Ship behind a flag until the migration finishes.\",\"valid_to_ms\":null,\"provenance\":[\"t1\"]}],\"worthless_turn_uids\":[]}",
];

/// 四条反例。第 1 条与第 2 条是 §2.11 对 §7 原表前两条的**改写**（注入块当证据 / 寒暄当事实），
/// 第 3、4 条原样。抽取侧不需要"时间相邻当因果"那一条：它一次只见一个批次，
/// 而 §2.11 要教的那一格是"这句根本不值得记"——它正是 worthless 的判据。
/// 槽位与 §7 原表编号的对应关系写在 §2.11 末尾，别把两套坐标读混。
const NEGATIVES: &[&str] = &[
    "BAD: treating the injected block as evidence. Lines starting with the injected marker are system output, not the user speaking; if a turn is only that, its id belongs in worthless_turn_uids.",
    "BAD: keeping a pleasantry as a fact. \"ok thanks\" and \"got it\" assert nothing; a turn that is only that belongs in worthless_turn_uids.",
    "BAD: writing your own inference as a fact (\"they seem understaffed\" is not a memory).",
    "BAD: introducing an entity that no turn names (a name you have never seen is not in the library).",
];

/// 三段形状的**唯一**拼装处（§2.11 末段：那句"共用一个组装函数"到本任务才真的存在）。
/// 参数顺序 = 渲染顺序，尾段是各自那份输出契约。
/// `extract_system()` 换到这里之后行为逐字节不变：任务 5 那五条断言一条都不许改。
pub(crate) fn assemble(
    canary: &str,
    head: &str,
    tail: &str,
    method: &[&str],
    standards: &[&str],
    positives: &[&str],
    negatives: &[&str],
) -> String {
    let mut s = String::new();
    s.push_str(canary);        // canary 只在模板首行出现一次（坑 P-09）
    s.push('\n');
    s.push_str(head);
    for line in method { s.push_str(line); s.push('\n'); }
    s.push_str("STANDARDS (all must hold):\n");
    for line in standards { s.push_str(line); s.push('\n'); }
    s.push_str("EXAMPLES:\n");
    for line in positives.iter().chain(negatives) { s.push_str(line); s.push('\n'); }
    s.push('\n');
    s.push_str(tail);
    s
}

pub(crate) fn shape_of(method: &[&str], standards: &[&str], positives: &[&str], negatives: &[&str]) -> PromptShape {
    PromptShape { method_steps: method.len(), standards: standards.len(),
                  positives: positives.len(), negatives: negatives.len() }
}

pub fn shape() -> PromptShape { shape_of(METHOD, STANDARDS, POSITIVES, NEGATIVES) }

/// `system` = 方法四步 + 标准四条 + 正例 1 + 反例 4，末尾拼输出契约（§7 原文的顺序）。
/// 拼装本身在 `assemble`，这一段顺序描述说的就是那一个函数体的顺序。
pub fn extract_system() -> String {
    assemble(EXTRACT_CANARY, HEAD_EXTRACT, TAIL_EXTRACT, METHOD, STANDARDS, POSITIVES, NEGATIVES)
}

const HEAD_EXTRACT: &str =
    "You are the extraction job of a memory center. You emit assertions, events, cards and entities and nothing else.\n";
/// 标题行现在在这一个常量里（`contract.rs` 的文档写了为什么它必须由契约层带着走）：
/// `concat!` 只吃字面量，prompt 层拼不出 "标题行 + 那个常量"，而 `format!` 绕就是把契约文本
/// 抄成第二个家。这里写成常量到常量的赋值，逐字节等于原来的 `concat!`。
const TAIL_EXTRACT: &str = crate::contract::OUTPUT_CONTRACT_SCHEMA;

/// 压缩摘要的**字面量约定**（B 作为候选 adapter 的输出约定）。识别的是正文里独立成行或
/// 开头的标记；标记出现在句子中间不算（避免把"讨论这个标记"的轮误认成摘要）。
/// 这条约定先按字面量落地，不依赖 B 是否已上线。
pub const COMPACT_SUMMARY_MARKER: &str = "[[compact_summary]]";

pub fn is_compact_summary(body: &str) -> bool {
    body.lines().any(|l| l.trim() == COMPACT_SUMMARY_MARKER)
        || body.trim_start().starts_with(COMPACT_SUMMARY_MARKER)
}

/// `user` = 原始 turn 逐条渲染 + "方括号里那串引用原样复制"。
/// 每条渲染前必过 `prepare`：剪掉的字节在 prompt 里**不存在**（不是"标为不可信"）。
/// 方括号里是**批内位置别名** `t1..tN`（编号规则住在 `batch::turn_ref`，解析侧同一个家），
/// 不是 64 位 uid：模型只需要抄一个两位的短引用，映射回真实轮由程序做。
/// 批内出现压缩摘要时（I1）：单独加一段抽取指令，点名 Key Decisions / Preferences /
/// Pending / User Messages 四类，并禁止把它列进 worthless。
pub fn extract_user(batch: &Batch) -> String {
    let mut s = String::new();
    let has_compact = batch.rows.iter().any(|r| is_compact_summary(&r.body));
    if has_compact {
        s.push_str("COMPACT SUMMARY TURNS (marked [[compact_summary]]): these are compression ");
        s.push_str("summaries, not chatter. Extract their Key Decisions, Preferences, Pending ");
        s.push_str("items and User Messages as ordinary cards/events/assertions, with the marked ");
        s.push_str("turn ref in provenance. If an ordinary turn restates a fact that also appears ");
        s.push_str("in a marked summary, cite both refs. NEVER put a marked turn in ");
        s.push_str("worthless_turn_uids.\n");
    }
    s.push_str("TURNS (ref in brackets, then role, then text):\n");
    for (index, r) in batch.rows.iter().enumerate() {
        let text = prepare(&r.body);
        s.push_str(&format!("[{}] ({}) {}\n", turn_ref(index), r.role, text));
    }
    s.push_str("Copy the bracketed ref(s) verbatim into provenance. Do not invent refs.\n");
    s
}

/// P-22 的批内预算：user 段的字符上限按行数算，超上限由调用方把批次切小（任务 6）。
pub const fn rendered_char_budget(rows: usize) -> usize {
    200 + rows * 4_000
}

// ── v4 A1（锚 / 提及 / 别名）──
const HEAD_A1: &str = "You are the anchoring pass (A1) of a memory center. You name the things the turns talk about and record how each turn refers to them. You do not extract facts, decisions, quotes or relations; a later pass does that.\n";

const METHOD_A1: &[&str] = &[
    "STEP 1 read every turn below in order; list each name a person would treat as a thing: a person, agent, organization, place, object, animal, work, event or concept.",
    "STEP 2 choose one canonical name per thing: the most specific stable name the source itself uses. Put every other spelling that appears in this batch into aliases exactly as written.",
    "STEP 3 apply the BOUNDARY TEST before keeping any candidate: a generic role, an unnamed room, a part, a scene ordinal, a time, a quantity or a single-turn prop is not an anchor. A proper-name place such as 书房 or 梧桐里 IS an anchor. Put a real referent in unresolved_surfaces; otherwise omit it. An excluded shape - a run of seven or more hex digits, a token in backticks, a file path, a full URL, a UUID, or a bare number - goes into no array at all: leave it out. unresolved_surfaces takes only a surface that IS a name but has no thing in this batch.",
    "STEP 4 apply the ONE-THING TEST: if two candidates share a name, one name is an alias of the other, or they are a full name / nickname / version of the same thing, keep one anchor and put the other surfaces in aliases. Never emit two anchors with the same canonical, even with different kinds.",
    "STEP 5 write one mention per turn per maximal surface: which anchor, which exact surface string, which turn id, who said it (speaker_ref), how the surface is used, and how sure you are. Do not also emit a substring of a surface you already emitted.",
    "STEP 6 when a surface cannot be tied to a thing from this batch alone, put it in unresolved_surfaces instead of inventing an anchor; when one name refers to two different things, emit two anchors with distinguishable canonical names and an issue of kind ambiguous_anchor; never merge them silently. The exclusion from STEP 3 holds here too: a hash fragment, a file path, a link, a backticked code token, a UUID or a bare number is not a name and goes into no array at all.",
];

const STANDARDS_A1: &[&str] = &[
    "NAMED, NOT IMPLIED: an anchor needs a surface that appears verbatim in a turn. \"a friend\" with no name is not an anchor; a named person is.",
    "EVIDENCE IN BATCH: every evidence entry is a turn id copied from this batch. An injected block or a compact summary is not a source of names.",
    "SAME NAME, DIFFERENT THING: two distinct things with the same name get two anchors. Disambiguate the canonical names with what the source says (role, place, relationship).",
    "ALIASES ARE VERBATIM: an alias must appear character-for-character in a turn of this batch. Do not translate it, shorten it, or add a title nobody says.",
    "CANONICAL IS VERBATIM TOO: use the exact phrase the turn uses for the specific name (测试组的李伟), without adding or dropping particles such as 的. The other verbatim surfaces go into aliases.",
    "SAME THING, DIFFERENT NAME: nicknames, full names and titles the source uses for one thing belong to one anchor. Different things never share an anchor.",
    "ONE DEVICE, ONE ANCHOR: when a turn gives a specific model or brand for a generic noun used nearby (打印机 then 惠普 M126a, 老电脑 then ThinkPad), the specific name is the canonical and the generic noun is an alias. Do not emit them as two objects of the same kind.",
    "NAMED DELIVERABLES COUNT: a schedule, document, report or checklist that the source names (搬家时间表) is an anchor of kind work, not only a fact for the later pass. Do not anchor generic categories (一个朋友) or unnamed instances (那台设备); a room, place, device or person the source names (书房) is still an anchor.",
    "NO FABRICATION: reality_status is unknown unless the source clearly says otherwise. fictional, virtual, abstract and hypothetical describe the world, not your guess.",
    "PRONOUN DISCIPLINE: pronouns such as 我, 你, 他, 她, 它, this, that or they can never be a canonical name or an alias. Resolve them through speaker_ref; if you cannot, use unresolved_surfaces.",
    "BOUNDARY TEST: a generic role, an unnamed room, a part, a scene ordinal, a date, a quantity or a one-off prop is not an anchor just because it is a noun. It needs a stable name a later memory sentence could use. 会议室, 物业, 行政, 房东, 前台, 椅子, 餐桌, 蛋糕, 包, 抽屉, 第二场, 第三场, 分片 7, 分片参数, 导出校验 and 302 的钥匙 are NOT anchors here; 书房, 梧桐里, 南风剧场 and 西湖区 ARE named places and remain anchors.",
    "ONE THING, ONE ANCHOR: full name + nickname + version label are one anchor. 方哲/老方, 南风剧场/南风剧场这版 and 云栖/云栖产品组 are one anchor each, with the shorter or generic surface in aliases. Never emit two anchors with the same canonical, even if their kinds differ.",
    "MAXIMAL SURFACE: for each turn and anchor emit only the longest surface; do not also emit a substring. For 2019 年的 ThinkPad emit that surface or ThinkPad, never both; for 测试组的李伟 do not also emit 测试组 as a separate mention of the same person.",
    "KIND FOLLOWS THE THING, NOT THE WORD: an agent or product that acts is kind agent; a source file, module, document, branch or release is work; a person's own name is person. Never file an acting agent as object, and never file a source file as org.",
    "ACCOUNT NAMES ARE NOT PEOPLE: a login name, a nickname with a numeric account id, a hostname, an OS user or a service username belongs to the environment, not to a person anchor. When only agent turns name a person, keep the anchor but answer reality_status unknown; never real.",
];

const POSITIVES_A1: &[&str] = &[
    "GOOD: (person) the turns say \"张三下周末搬家\" and \"小张说她自己买的那瓶饮料\" -> {\"ref\":\"a1\",\"kind\":\"person\",\"canonical\":\"张三\",\"aliases\":[\"小张\"],\"reality_status\":\"real\",\"evidence\":[both turn ids],\"reason\":\"原文点名且以别名称呼\"}, plus one mention per turn with surface 张三 / 小张 and speaker_ref a1.",
    "GOOD: (object) \"老电脑重装完了，还差一次批量验证\" -> {\"ref\":\"a2\",\"kind\":\"object\",\"canonical\":\"老电脑\",\"aliases\":[],\"reality_status\":\"real\",\"evidence\":[that turn id],\"reason\":\"原文点名的设备\"}; the surface 老电脑 is mentioned, not a new fact.",
    "GOOD: (one person, two names) \"方哲说先别改\" and \"老方在群里说\" -> one person anchor {\"canonical\":\"方哲\",\"aliases\":[\"老方\"]}, not two anchors.",
    "GOOD: (one event) \"季度复盘会\" and \"复盘会那场\" -> one event anchor {\"canonical\":\"季度复盘会\",\"aliases\":[\"复盘会\"]}, even though one turn calls it 那场.",
];

const NEGATIVES_A1: &[&str] = &[
    "BAD: an anchor whose canonical is 我, 你, 他, 她, 它 or a bare pronoun - those are mentions at most, never names.",
    "BAD: translating an alias. If the turn says 小张, the alias is 小张; writing wife invents a string that is not in the source.",
    "BAD: an anchor for a generic category (\"一个朋友\", \"公司\", \"那台打印机\") when no turn gives a stable name; put the surface in unresolved_surfaces instead.",
    "BAD: using an injected marker block or a compact summary as evidence, or naming something only that block mentions; those lines are system output, not the human speaking.",
    "BAD: two object anchors \"打印机\" and \"惠普 M126a\" for one printer, or \"老电脑\" and \"ThinkPad\" for one computer. Keep the specific name as canonical and put the generic surface in aliases of the same anchor.",
    "BAD: anchoring 会议室, 物业, 行政, 房东, 前台, 椅子, 餐桌, 蛋糕, 包 or 抽屉 just because they are nouns. They are unnamed roles, rooms or props: do not anchor them.",
    "BAD: anchoring 第二场, 第三场, 第三稿, 分片 7, 分片参数, 导出校验 or 主版本 as concept/work/object. Scene ordinals, parts and checks are not entities.",
    "BAD: emitting 方哲 and 老方 as two person anchors, 南风剧场 and 南风剧场这版 as two anchors, 云栖 and 云栖产品组 as two anchors, or two anchors with the same canonical but different kinds. Merge them into one anchor with aliases.",
    "BAD: emitting both 2019 年的 ThinkPad and ThinkPad, or both 设计组的林秋白 and 林秋白, in the same turn. Emit only the maximal surface.",
    "BAD: anchors whose canonical is a line range, a log path carrying a line number, or a sentence that only says what to do next (第 40–52 行, app.log:12, 先把清单导出). Positions, paths inside a file and instructions are not things a later memory sentence names.",
    "BAD: reality_status real for a person whose every evidence turn is an agent merely mentioning the name; keep the anchor and answer unknown.",
    "BAD: filling unresolved_surfaces with hashes, paths, URLs, code tokens or numbers to avoid leaving it empty. An empty array is a legal answer when no name passes the boundary test.",
];

const TAIL_A1: &str = "OUTPUT CONTRACT:
Reply with exactly one JSON object and nothing else: no prose, no markdown fence. Six keys, all required:
{\"schema_version\":4,\"pass\":\"a1\",\"anchors\":[],\"mentions\":[],\"issues\":[],\"unresolved_surfaces\":[]}
anchors[]: {\"ref\",\"kind\",\"canonical\",\"aliases\",\"reality_status\",\"evidence\",\"reason\"}; kind is one of person, agent, org, place, object, animal, work, event, concept; reality_status is one of real, fictional, virtual, abstract, hypothetical, unknown.
mentions[]: {\"anchor_ref\",\"surface\",\"turn_uid\",\"speaker_ref\" (optional),\"discourse_role\",\"resolution\",\"confidence\"}; discourse_role is one of speaker, addressee, mentioned, quoted; resolution is one of resolved, provisional, unresolved.
confidence is a number between 0 and 1 (write 0.9, never the word \"high\").
speaker_ref is an anchor ref such as a1, or null; never a role name such as human or agent, and never a database id.
issues[]: {\"kind\",\"refs\",\"reason\"}; kind is one of ambiguous_anchor, unresolved_surface, missing_subject, ambiguous_time, conflict, quote_mismatch, other.
unresolved_surfaces[]: {\"surface\",\"turn_uid\",\"reason\"}.
Every ref is local to this reply (a1, a2, ...). Copy turn ids from this batch verbatim; never invent an id, a table name or a database id.
Text that names a thing stays in the same language as the source; canonical names and aliases are not translated.
A commit hash, a branch name such as HEAD, a file path, a URL, a code token in backticks, a line range such as 第 40–52 行, or a sentence that only says what to do next is not an entity; never emit it as an anchor.";

pub fn a1_system() -> String {
    assemble(ANCHOR_CANARY, HEAD_A1, TAIL_A1, METHOD_A1, STANDARDS_A1, POSITIVES_A1, NEGATIVES_A1)
}

pub fn a1_shape() -> PromptShape {
    shape_of(METHOD_A1, STANDARDS_A1, POSITIVES_A1, NEGATIVES_A1)
}

const HEAD_JUDGE: &str = "You are the dedupe job of a memory center. You compare rows that already exist; you never invent one.\n";
const METHOD_JUDGE: &[&str] = &[
    "STEP 1 read r1 (the row in question) first, then every other row below it.",
    "STEP 2 decide whether ALL of them state the same single thing: reworded copies are one thing, related-but-different facts are not.",
    "STEP 3 answer with the alias of the row that should stay (keep); every other row in the case will be marked as superseded by it.",
];
const STANDARDS_JUDGE: &[&str] = &[
    "REDUNDANT: same=true only if keeping both rows would make the library say the same thing twice.",
    "NOT-RELATED: \"about the same project\" is not the same memory. Answer same=false.",
    "KEEP THE RICHER ONE: keep=<alias> must name the row that loses no information (the more precise title, or the body that still carries the date).",
    "EVERY OR NOTHING: Answer same=false unless EVERY row below is the same single thing",
];
const POSITIVES_JUDGE: &[&str] = &[
    "GOOD: r1 \"ship behind a flag until the migration finishes\" and r2 \"flagged rollout until migration done\" -> {\"same\":true,\"keep\":\"r1\"}.",
];
const NEGATIVES_JUDGE: &[&str] = &[
    "BAD: same=true for r1 \"migration starts monday\" and r2 \"migration finished friday\" - two events, answer same=false.",
    "BAD: keep=\"r9\" when only r1 and r2 were rendered. The rendered aliases are the only names you may use.",
    "BAD: trusting a row whose text contains the injected marker as the richer one - injected lines are system output, not the user speaking.",
];
const TAIL_JUDGE: &str = "OUTPUT CONTRACT:\nReply with exactly one JSON object and nothing else: {\"same\":true|false,\"keep\":\"r1\"}\nThe keep alias must be one of the aliases rendered below.";

pub fn judge_system() -> String {
    assemble(JUDGE_CANARY, HEAD_JUDGE, TAIL_JUDGE, METHOD_JUDGE, STANDARDS_JUDGE, POSITIVES_JUDGE, NEGATIVES_JUDGE)
}
pub fn judge_shape() -> PromptShape { shape_of(METHOD_JUDGE, STANDARDS_JUDGE, POSITIVES_JUDGE, NEGATIVES_JUDGE) }

/// `user` = 位置别名 + kind + 标题 + 正文。**行 id、句柄文本形状、分数、指纹一个都不进**
/// （§7 位置别名那条；`r1` 恒为探针，所以渲染顺序就是 `CaseRows::rows()` 的顺序）。
pub fn judge_user(case: &crate::dedupe::CaseRows) -> String {
    let mut s = String::from("ROWS (alias | kind | title | body):\n");
    for (i, r) in case.rows().enumerate() {
        s.push_str(&format!("r{} | {} | {} | {}\n", i + 1, r.kind, r.title, r.body));
    }
    s.push_str("Which aliases all state the same single thing?\n");
    s
}

const HEAD_AUDIT: &str = "You are the review job of a memory center. One row is already in the library; you judge it, you never rewrite it.\n";
const METHOD_AUDIT: &[&str] = &[
    "STEP 1 read the row in question, then answer exactly the one question asked below.",
    "STEP 2 use only what this prompt gives you: the row, and for this one vote the context rendered with it.",
    "STEP 3 answer holds=true when the stated condition holds, holds=false when it does not. One boolean, nothing else.",
];
const STANDARDS_AUDIT: &[&str] = &[
    "ONE QUESTION ONLY: this pass asks one of three questions. Do not answer a question that is not printed here.",
    "STATED, NOT INFERRED: if the text does not say it, it is not there. holds=false is the honest answer to a question you cannot answer.",
    "FALSE ABOUT CONTENT: holds=false must be about the memory's own content. A row you find boring, badly worded or unimportant is still a valid answer to whatever was asked.",
    // R2-1：票 3 的邻居池可能是"不可用"（探针无归属 ⇒ 归属过滤把整池清空）。那与"查过、
    // 没有相关邻居"是两件事。这一票的契约只有 bool，没有 uncertain 这一态，所以用常量里
    // 的布尔版本（判不了 ⇒ holds=false）；多态契约的 pass 用带 uncertain 的那一条。
    crate::pass_prompt::ABSENT_NEIGHBORS_HOLDS,
    "SHAPE: exactly one JSON object, key holds, boolean value. No prose, no second key, no markdown fence.",
];
const POSITIVES_AUDIT: &[&str] = &[
    "GOOD: asked \"vote2-long-lived\" about \"the build breaks without the migration flag\" -> {\"holds\":true}.",
];
const NEGATIVES_AUDIT: &[&str] = &[
    "BAD: {\"holds\":\"true\"} - a quoted boolean is a string, and a string here is a broken answer, not a no.",
    "BAD: answering vote1's question about provability when vote3's question about contradiction is the one printed.",
    "BAD: {\"holds\":true,\"note\":\"...\"} - the contract is one key. Extra keys make the whole reply unreadable.",
];
const TAIL_AUDIT: &str = concat!(
    "OUTPUT CONTRACT:\n",
    "Reply with exactly one JSON object and nothing else: {\"holds\":true|false}"
);

pub fn audit_system() -> String {
    assemble(AUDIT_CANARY, HEAD_AUDIT, TAIL_AUDIT, METHOD_AUDIT, STANDARDS_AUDIT, POSITIVES_AUDIT, NEGATIVES_AUDIT)
}
pub fn audit_shape() -> PromptShape { shape_of(METHOD_AUDIT, STANDARDS_AUDIT, POSITIVES_AUDIT, NEGATIVES_AUDIT) }

/// user 段 = 票号 + 那一票的问题 + 被审的行，**再按这一票需不需要上下文**拼证据或邻居。
/// 三段上下文的分派是 `Vote` 上的两个谓词，不是这里的 `if i == 0`：票与它读什么是同一件事的两面，
/// 拆开两处就会漂移（P-43 的"文本各不相同、但都没有证据"那一型正是漂移的症状）。
/// 位置别名在这里**不需要**（判同要在 r1/r2 之间选 keep，审核只判一行），
/// 所以渲染里没有 id、没有句柄、没有分数、没有归属键（守卫 12 管出口，这条是入口侧的自律）。
/// 票号只在第一行出现那一次：数据行的第一格是 `row` / `neighbour` 这种**角色名**，不是票号——
/// 把票号铺进每一行，"三段的票号互相串"那一族就退化成只测第一段。
pub fn audit_user(case: &crate::audit::AuditCase, vote: crate::audit::Vote) -> String {
    let mut s = format!("{}: {}\n", vote.slug(), vote.question());
    s.push_str("ROW | kind | title | body:\n");
    s.push_str(&format!("row | {} | {} | {}\n", case.row.kind, case.row.title, case.row.body));
    if vote.renders_evidence() && !case.evidence.is_empty() {
        s.push_str("EVIDENCE (turn role | body):\n");
        for (role, body) in &case.evidence {
            s.push_str(&format!("{role} | {body}\n"));
        }
    }
    if vote.renders_siblings() {
        if case.siblings_unavailable {
            // R2-1：探针**没有归属行** ⇒ `rows_in_scope` 的 `?6=1` 把整池清空。这时的"没有
            // 邻居"是**信息缺失**，不是"查过、没有矛盾"——明写一行，别让模型把空池当证据。
            s.push_str("NEIGHBORS: unavailable (no owner for this probe)\n");
        } else if !case.siblings.is_empty() {
            s.push_str("NEIGHBOUR | kind | title | body:\n");
            for r in &case.siblings {
                s.push_str(&format!("neighbour | {} | {} | {}\n", r.kind, r.title, r.body));
            }
        }
    }
    s.push_str("Answer for this single vote.\n");
    s
}
