//! 推导 prompt（架构 §7 原文 + 守卫 3 形状只有一份）。
//! canary 串只住在这里（坑 P-09）；assemble 复用 `rc-distill::prompt::assemble` 的形状（不重写，公共拼装路径只一处）。
//! 反例四支 = 架构 §7"推导 prompt 的三段"原文固定：时间相邻当因果 / 同现实体当关系 / 推测当事实 / 造新实体；
//! 批次 M 段 B 在四支之后追加第 5 条（mentions 反例：代词 / 非逐字名字），四支原文一字不动；
//! 派发档 I-2（Phase 2.4 · 证据闸）再追加第 6 条（套话机制 / 零依据行），前五条原文一字不动。
//! STEP 4 里那句 "MECHANISM SENTENCE" 是本闸的**问**的一侧：程序闸（`parse` 的两道 +
//! `writers_x::lacks_mechanism_evidence`）拦的是答不上来的那一支，而 prompt 不要求它，
//! 闸就天天红、模型天天答不上——排除条款要配程序闸，反过来程序闸也要配排除条款。
//! 行 id / 句柄 / 分数 / 指纹一个都不进 user 段（与 judge_user 同律）。

use rc_memory::RowFacts;

use crate::date::local_ms_label;

/// 四段计数。字段与 `rc_distill::prompt::PromptShape` 同形，但**不 import 它**：
/// T5 让 `rc-distill` 依赖 `rc-derive`，反向依赖会成环（cargo 直接拒绝），
/// 所以这里是一个本地镜像类型——被钉死的从来是"四段计数"这个形状，不是那个 type 名。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptShape {
    pub method_steps: usize,
    pub standards: usize,
    pub positives: usize,
    pub negatives: usize,
}

pub const DERIVE_CANARY: &str = "__DERIVE_CANARY_6f3a__";

const HEAD_DERIVE: &str =
    "You are the derivation job of a memory center. You compare rows that are already in the library; you never invent one and you never quote a turn.\n";

const METHOD: &[&str] = &[
    "STEP 1 consider only pairs of distinct rows that share at least one entity name or an explicit textual reference (a word that appears verbatim in both rows' title or body). Do not enumerate unrelated pairs; if the two rows share no such anchor, drop the pair without analysis.",
    "STEP 2 a shared entity is not a relation by itself. Write a relation only when one row's text states a directed relation (A depends on B, A is part of B, A owns/located_in/role B) that you can re-state in one sentence from the two rows' text only. If the text only shows co-occurrence or same-thing, drop the pair.",
    "STEP 3 for each timeline, the date or offset must come from the row's own text (absolute date) or from another row (relative offset). An absolute date must appear verbatim in one basis row's title or body; a date you can only infer is unknown. Give either date or anchor+offset_ms, never both.",
    "STEP 4 output the four keys (relationships, chains, timeline, mentions). Missing any of the first three is a contract violation. An empty value is allowed only when the relevant question has no answer. A chain lists every link in order (at least two rows) and carries its type. Every chain must also carry a MECHANISM SENTENCE: one sentence, restated from the rows you were shown, that says why the link before it brings about, enables or blocks the next one, and it must name at least one basis row (an empty 'basis' array is not a chain). That sentence is the evidence the library keeps for the edge; when you cannot write it, the chain does not exist, so drop it instead of filling the field. Write every reason, mechanism and body_ref in the language of the rows you were shown (never translate them), and drop a chain whose mechanism merely repeats one row's sentence instead of restating the row.",
];

const STANDARDS: &[&str] = &[
    "CLOSED ENUM: relationship kind is one of part_of, depends_on, owns, located_in, role; chain type is one of causes, enables, prevents. Anything else is rejected, not rewritten.",
    "NO CO-OCCURRENCE: a relation is not 'they appear in the same week', 'they were mentioned together', or 'they share an entity'. A shared entity only narrows the candidate pairs; co_occurs is a rejection, not a relation. For chains the same bar is higher: a chain is a rejection too when its mechanism is a placeholder ('time adjacency', 'same entity', 'same turn') or when it names no basis row — no evidence, no chain.",
    "ROW ONLY: do not import a turn, a quote, a fingerprint, a handle or a score into the basis. The basis is a list of row aliases (r1..rn). A mention names one row alias and a name copied verbatim from that row's title or body.",
    "SINGLE BASIS: one relation, one chain, one timeline each. Do not merge two findings into one item.",
];

const POSITIVES: &[&str] = &[
    "GOOD: r1 'project alpha uses postgres' and r2 'project beta mirrors alpha' -> relationships: [{from:r1, to:r2, kind:part_of, reason:r2 says it mirrors, basis:[r1,r2]}], chains: [{from:[r1,r2], mechanism:r2 mirrors r1, type:causes, basis:[r1]}], mentions: [{row:r1, name:postgres}].",
    "GOOD: r1 'the cap decision removed the old size threshold' and r2 'fallback no longer triggers by size' -> chains: [{from:[r1,r2], mechanism:because the cap decision removed the size threshold, fallback cannot trigger by size, type:causes, basis:[r1,r2]}]. That mechanism sentence restates what the two rows say about each other, and the basis names the rows it is restated from — both are there, so the chain is emittable.",
];

// 反例四支（架构 §7"推导 prompt 的三段"原文）+ 批次 M 段 B 追加的 mentions 一条
// + 派发档 I-2（Phase 2.4 · 证据闸）追加的"套话机制 / 零依据行"一条
const NEGATIVES: &[&str] = &[
    "BAD: writing depends_on for two rows whose only commonality is that they were created in the same week. Time-adjacency is co-occurrence, not cause.",
    "BAD: writing part_of for two rows about the same real-world entity but with different scopes. Same-thing is dedupe, not a relation.",
    "BAD: writing a relation whose basis is your own inference ('they must be related because...'). If the text does not say it, drop the pair.",
    "BAD: writing a relation that names an entity not present in any rendered row. A name you have not been shown is not in the library.",
    "BAD: writing a mention for a pronoun or for a name that is not copied verbatim from that row's title or body. If the name is not in the row text, drop the mention.",
    "BAD: writing a chain whose mechanism is 'same turn', 'adjacent in time' or 'the same entity', or whose basis array is empty. A placeholder mechanism names the pairing, not the cause, and a chain with no basis row has nothing to be re-computed from — drop both.",
];

const TAIL_DERIVE: &str = concat!(
    "OUTPUT CONTRACT:\n",
    "Reply with exactly one JSON object and nothing else:\n",
    "{\"relationships\":[{\"from\":\"r1\",\"to\":\"r2\",\"kind\":\"part_of\",\"reason\":\"...\",\"basis\":[\"r1\"]}],\n",
    " \"chains\":[{\"from\":[\"r1\",\"r2\"],\"mechanism\":\"...\",\"type\":\"causes\",\"basis\":[\"r1\"]}],\n",
    " \"timeline\":[{\"anchor\":\"r1\",\"offset_ms\":86400000,\"body_ref\":\"...\",\"basis\":[\"r1\"]},\n",
    "             {\"date\":\"2026-10-15\",\"body_ref\":\"...\",\"basis\":[\"r2\"]}],\n",
    " \"mentions\":[{\"row\":\"r1\",\"name\":\"...\"}]}\n",
    "relationships, chains and timeline are all required; mentions is additive and may be empty. ",
    "A chain is emitted only with a mechanism sentence and at least one basis row; a chain without either is dropped by the reader, so do not pad those two fields. ",
    "A mention name must be copied verbatim from the row's title or body; prefer a name that also appears as an entity row, otherwise omit it. ",
    "at= is the row's recorded time: use it only to order rows. A timeline date must still be copied verbatim from a basis row's title or body; a date taken from at= is rejected. ",
    "Reply with the JSON object only; do not explain or enumerate pairs in the content."
);

/// 形状计数（**不**调 `rc_distill::prompt::assemble`——那会让 rc-derive 反向依赖 rc-distill、
/// 与 T5 的正向依赖成环；这里只把"段"钉死，拼装是本地四段 `push_str`）。
pub fn shape() -> PromptShape {
    PromptShape { method_steps: METHOD.len(), standards: STANDARDS.len(),
                  positives: POSITIVES.len(), negatives: NEGATIVES.len() }
}

pub fn derive_system() -> String {
    // 不引 rc_distill::prompt::assemble（避免循环依赖）；本地最小化版本：
    let mut s = String::new();
    s.push_str(DERIVE_CANARY); s.push('\n');
    s.push_str(HEAD_DERIVE);
    for line in METHOD { s.push_str(line); s.push('\n'); }
    s.push_str("STANDARDS (all four must hold):\n");
    for line in STANDARDS { s.push_str(line); s.push('\n'); }
    s.push_str("EXAMPLES:\n");
    for line in POSITIVES.iter().chain(NEGATIVES) { s.push_str(line); s.push('\n'); }
    s.push('\n');
    s.push_str(TAIL_DERIVE);
    s
}

/// user 段 = 位置别名 + kind + title + body。
/// **行 id、句柄、分数、指纹一个都不进**（与 judge_user 同律：守卫 12 的入口侧自律）。
/// r1 = rows[0]、r2 = rows[1]……一一对应、绝不重号。
/// user 段 = 位置别名 + kind + 行钟 + 参与者 + title + body。
/// **行 id、句柄、分数、指纹一个都不进**（与 judge_user 同律：守卫 12 的入口侧自律）。
/// r1 = rows[0]、r2 = rows[1]……一一对应、绝不重号。
pub fn derive_user(rows: &[RowFacts]) -> String {
    derive_user_with(rows, &[])
}

/// 带参与者的一版：`participants[i]` 对应 `rows[i]`；缺项渲染 `who=unknown`（不猜）。
pub fn derive_user_with(rows: &[RowFacts], participants: &[Vec<String>]) -> String {
    // `events` 全空 = 每行都回落 `as_of_ms`（`derive_user`/`derive_user_with` 的既有形状，
    // 派发档 I-7 之前的唯一形状，用例在 `tests/prompt_shape.rs`）。
    derive_user_with_events(rows, participants, &[])
}

/// 事件时间版（债 #9 · 修法甲）：`events[i]` 对应 `rows[i]` 的 `card.time_json` 事件时间
/// （`kind='event_time'` 的 `start_ms`，装载在 `derive.rs` 经 `rc_memory::reader::card_event_times`）。
/// 在场 ⇒ `at=` 用事件钟；缺席 ⇒ 回落 `as_of_ms`（现状链一字不动）。**判据只认格**：
/// 锚行没有 `time_json`，调用方给它的必然是 `None`。
pub fn derive_user_with_events(rows: &[RowFacts], participants: &[Vec<String>],
                               events: &[Option<i64>]) -> String {
    let mut s = String::from("ROWS (alias | kind | at | who | title | body):\n");
    for (i, r) in rows.iter().enumerate() {
        let who = participants.get(i)
            .filter(|names| !names.is_empty())
            .map(|names| names.join(", "))
            .unwrap_or_else(|| "unknown".to_string());
        // `at=` 的口径（派发档 I-7 契约 4）：card 行优先 `time_json` 事件时间，无则回落
        // `as_of_ms`。TAIL_DERIVE 那句 "at= is the row's recorded time" 的语义不变——
        // 换的是钟的来源，不是它"只用于排序"的用途。
        let at = events.get(i).copied().flatten().unwrap_or(r.as_of_ms);
        s.push_str(&format!("r{} | {} | at={} | who={} | {} | {}\n",
                            i + 1, r.kind, local_ms_label(at), who, r.title, r.body));
    }
    s.push_str("Reply with the four keys. Empty arrays are allowed for keys with no findings.\n");
    s
}
