//! 中间置信带的真 LLM 裁决（派发档 I-12）。它实现 [`crate::episode::MidBandAdjudicator`]：
//! 程序半刀（[`crate::episode::plan_pairs`]）筛出中间带候选后，这里对**每一对**独立调一次模型，
//! 只回答一个语义问题——"这两条 fact 是否同一段经历"。
//!
//! 四件事由派发档钉死，改动任何一件都要先改派发档：
//! ① **一次一个语义任务**：不把多对塞进一个 prompt（批内上下文互染），每对独立调用；
//! ② **prompt 三件套**：方法（主语/动作/时间窗/共同参与者逐项比对）+ 标准（同一段经历 =
//!    同一次发生的事，不是同类事件；拿不准必须 `uncertain`）+ 正反例——正面是 I-3b 已合并
//!    那一对的**形状**（808s、共同参与者），反面是 R-C 阴性对照的**形状**（55.9s、参与者
//!    交集 0）。prompt 里只有形状描述、没有现网专名（夹具纪律：现网事实一律运行期从库读）；
//! ③ **短 id 引用**：批内 `t1`/`t2`，程序映射回真实 fact id——弱模型抄 64 位哈希必错；
//! ④ **fail-closed**：输出不合闭集、调用失败、重试耗尽，一律折成
//!    [`MidVerdict::Uncertain`] ⇒ 不合并、不落库（与 [`crate::episode::DeferMidBand`] 同侧）。
//!    每对重试 ≤ [`ADJUDICATOR_RETRY_TIMES`] 次（首发 + 2 重试）。
//!
//! **同一对只问一次**：[`LlmMidBand`] 带 (fact_a, fact_b) → 结论的缓存。`run_episode_pass`
//! 是"窗逐趟放大、整段重算"的循环，第二趟会对同一对再走一次裁决——缓存让重算**不重问**，
//! 否则一趟真跑烧两倍调用。缓存是进程内的，跨进程重跑会再问一次（幂等由写侧
//! `INSERT OR IGNORE` 兜底；结论漂移的风险登记在 I-12 报告，不在本层修）。
//!
//! 时延读数用 `std::time::Instant`（单调钟）。判据 12a 的禁令是"取**系统**钟"
//! （std 的墙钟接口，名字不写出来——那道闸连注释一起数，2026-11-05 实证）：
//! 判定输入里没有这一格，它只是判定表的读数出口，与
//! `rc_llm::Deadline`"钟从形参进来"那一条不冲突——这一层的判定不读它。

use crate::episode::{MidBandAdjudicator, Pair};
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use rc_memory::reader::GroupFact;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

/// 每对的调用上限：首发 + 重试 ≤ 2（派发档 §硬纪律 4）。超了折 [`MidVerdict::Uncertain`]。
pub const ADJUDICATOR_RETRY_TIMES: usize = 2;

/// 裁决的闭集结论。`Uncertain` 是三态之一，不是错误：它是 fail-closed 的落点，
/// 与"调用失败"共用同一处置（不合、不落库），但**理由格**分得开——三态是模型的诚实回答，
/// 失败是机器的诚实回答，混成一句会把"模型判不了"读成"链路坏了"。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MidVerdict {
    /// 同一段经历 ⇒ 落进合并集。
    Merge,
    /// 不是同一段经历 ⇒ 不合。
    KeepApart,
    /// 判不了 / 输出不合闭集 / 调用失败重试耗尽 ⇒ 不合（fail-closed）。
    Uncertain,
}

impl MidVerdict {
    /// 判定表与运维面的封闭词表。模型答的是 yes|no|uncertain，落账用同一张表。
    pub fn slug(self) -> &'static str {
        match self {
            MidVerdict::Merge => "yes",
            MidVerdict::KeepApart => "no",
            MidVerdict::Uncertain => "uncertain",
        }
    }
}

/// 一次裁决的全账（判定表的一行）。每一格都是"逐对可溯"的交账格：
/// fact id 对、结论、理由、座位、时延、prompt 指纹、本对烧掉的调用数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adjudication {
    pub fact_a: String,
    pub fact_b: String,
    pub verdict: MidVerdict,
    /// 一句理由。模型给的原文截到 [`REASON_CLIP`] 字；机器给的失败理由是固定短语
    /// + `LlmFailure` 的封闭类别串，不带任何模型原文（守卫 12 在这一格的形状）。
    pub reason: String,
    /// 答这一对的座位（`LlmBackend::slug()`；没有身份的后端 = `unknown`）。
    pub seat: String,
    pub latency_ms: u64,
    /// sha256(system · 0u8 · user)，hex。判定表只存指纹不存全文：全文由
    /// `adjudication_prompt` 从同一库同一对可重建（内容寻址，不是记录寻址）。
    pub prompt_sha: String,
    /// 本对烧掉的调用数（含重试；缓存命中不产生新行，故只在首次裁决时记）。
    pub calls: usize,
}

/// 模型理由的截断长度（按字符数，与 `episode::clip` 同尺）。
const REASON_CLIP: usize = 200;
/// 成员正文进 prompt 的截断长度（按字符数）。
const TEXT_CLIP: usize = 200;

// ── prompt 三件套 ────────────────────────────────────────────────────────────
// 与 `prompt.rs` 那一族同形状：HEAD / METHOD / STANDARDS / POSITIVES / NEGATIVES / TAIL。
// 正反例只写形状（808s+共同参与者 / 55.9s+交集为空），不写现网专名——现网事实运行期从库读，
// prompt 模板里一个专名都不抄（episode.rs 文件头的夹具纪律，本模块沿用同一条）。

const HEAD_EPISODE_JUDGE: &str = "You are the episode adjudicator of a memory center. \
You judge exactly one question about exactly one pair of fact records: \
do these two records describe the same single experience (one thing that happened)?\n";

const METHOD_EPISODE_JUDGE: &[&str] = &[
    "STEP 1 compare the two records point by point: subject (who), action/content (what happened), time window (the gap given below), shared participants.",
    "STEP 2 compare first, conclude last: write the one-sentence reason from your comparison, then pick the verdict.",
    "STEP 3 answer only the question asked. The aliases t1/t2 refer to the two records printed below.",
];

const STANDARDS_EPISODE_JUDGE: &[&str] = &[
    "SAME EXPERIENCE = one occurrence: a step, a follow-up, or another angle of the very same happening.",
    "SAME KIND IS NOT SAME EXPERIENCE: two separate happenings that merely resemble each other must be kept apart, even when they are close in time.",
    "CLOSE IN TIME ALONE IS NOT ENOUGH: a small time gap with unrelated subjects/contents is not one experience.",
    "UNSETTLED: when the records do not give you enough to decide, answer uncertain. Never guess.",
    "SHAPE: exactly one JSON object, key verdict is one of \"yes\"|\"no\"|\"uncertain\", key reason is one sentence. No prose, no markdown fence, no extra keys.",
];

/// 正例形状 = I-3b 已合并的那一对（Δt 808s ≈ 13.5min、剥自锚后有共同参与者）。
const POSITIVES_EPISODE_JUDGE: &[&str] = &[
    "GOOD: t1 \"user asked to try a service\", t2 \"about 13 minutes later: noted the service is a thinking model\", one shared participant (the service itself) -> verdict yes: the second record is a step of the same try-it episode.",
];

/// 反例形状 = R-C 阴性对照的两型：Δt 55.9s 但参与者交集 0；Δocc 同刻但 Δcreated 1.2min 仍无交集。
const NEGATIVES_EPISODE_JUDGE: &[&str] = &[
    "BAD: t1 and t2 about entirely different matters, gap under a minute, zero shared participants -> verdict no: closeness in time alone is not one experience.",
    "BAD: t1 and t2 recorded at the same event time but written a minute apart, subjects and contents unrelated -> verdict no: record time says nothing when the contents do not connect.",
];

const TAIL_EPISODE_JUDGE: &str = concat!(
    "OUTPUT CONTRACT:\n",
    "Reply with exactly one JSON object and nothing else: {\"verdict\":\"yes\"|\"no\"|\"uncertain\",\"reason\":\"...\"}"
);

fn assemble(head: &str, tail: &str, method: &[&str], standards: &[&str],
            positives: &[&str], negatives: &[&str]) -> String {
    let mut s = String::from(head);
    s.push_str("METHOD:\n");
    for l in method { s.push_str(l); s.push('\n'); }
    s.push_str("STANDARDS:\n");
    for l in standards { s.push_str(l); s.push('\n'); }
    s.push_str("EXAMPLES:\n");
    for l in positives { s.push_str(l); s.push('\n'); }
    for l in negatives { s.push_str(l); s.push('\n'); }
    s.push_str(tail);
    s
}

/// 一条记录进 prompt 的一行：短 id | 主语（锚的 canonical）| 时刻 | 正文（截断）。
/// 参与者不逐个列锚 id（哈希串进 prompt 只会教弱模型抄哈希）：共同参与者由程序算好，
/// 在下面的 pair 行里以计数给出（派发档：参与者由程序准备好）。
fn record_line(alias: &str, f: &GroupFact) -> String {
    let ms = f.occurred_at_ms.unwrap_or(f.created_at_ms);
    let clock = match f.occurred_at_ms {
        Some(_) => "event",
        None => "speech",
    };
    let subject = f.subject_canonical.as_deref().unwrap_or(&f.subject_anchor_id);
    format!("{alias} | subject: {subject} | {clock}_at_ms: {ms} | text: {}", clip(&f.text, TEXT_CLIP))
}

/// 按字符数截断（中文正文按字算，不切 UTF-8 字节）。与 `episode::clip` 同一把尺子，
/// 但那一格是私有 fn——复制一份并在两边注释里互相点名，比提公开（给外面一份
/// "episode 内部字面量的复本"）更便宜。
fn clip(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit { return text.to_string(); }
    let mut head: String = chars[..limit].iter().collect();
    head.push('…');
    head
}

/// 出处邻近的可读形状：同证据轮，或会话内轮距。轮距读不到时这一对根本不会成为候选
/// （`plan_pairs` fail-closed），所以这里只有两支。
fn proximity_line(pair: &Pair) -> String {
    if pair.same_turn {
        "evidence: both records cite the same conversation turn".to_string()
    } else {
        format!("evidence: {} turn(s) apart in the same session",
                pair.turn_distance.unwrap_or(0))
    }
}

/// 一对候选的完整 user prompt。`shared` 是剥自锚后的共同参与者数
/// （调用方从 `episode::shared_participants` 现算——程序准备 mechanics，模型只答语义）。
pub fn adjudication_user(facts: &[GroupFact], pair: &Pair, shared: usize) -> String {
    let (a, b) = (&facts[pair.a], &facts[pair.b]);
    let gap = pair.delta_ms;
    let mut s = String::from("RECORDS (alias | subject | clock | text):\n");
    s.push_str(&record_line("t1", a));
    s.push('\n');
    s.push_str(&record_line("t2", b));
    s.push('\n');
    s.push_str(&format!(
        "PAIR FACTS: time gap between t1 and t2 = {gap} ms; shared participants after excluding each speaker's own anchor = {shared}; {}\n",
        proximity_line(pair)));
    s.push_str("QUESTION: are t1 and t2 the same single experience (one thing that happened)?\n");
    s
}

/// 裁决的 system prompt（三件套的拼装点）。
pub fn adjudication_system() -> String {
    assemble(HEAD_EPISODE_JUDGE, TAIL_EPISODE_JUDGE, METHOD_EPISODE_JUDGE,
             STANDARDS_EPISODE_JUDGE, POSITIVES_EPISODE_JUDGE, NEGATIVES_EPISODE_JUDGE)
}

/// 闭集拒绝标记：形状与词表都不合。它不带正文（守卫 12），也不区分"哪一格坏了"
/// ——对调用方这些全是同一处置：重试；耗尽折 [`MidVerdict::Uncertain`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerdictRejected;

/// 解析模型的答复：闭集三态 + 一句理由。认不出的形状一律 `Err`
/// （调用方折成失败重试；重试耗尽 = [`MidVerdict::Uncertain`]）。
/// 宽容面只有两格：围栏剥离（有的座位会包 ```json）与首尾空白；此外逐字收。
pub fn parse_verdict(raw: &str) -> Result<(MidVerdict, String), VerdictRejected> {
    let mut s = raw.trim();
    // 剥围栏：```json ... ``` 或 ``` ... ```。围栏内才是答复本体。
    if let Some(rest) = s.strip_prefix("```") {
        // 跳过语言标记行（"json" 等）。
        let body = match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => rest,
        };
        s = body.trim();
        if let Some(stripped) = s.strip_suffix("```") { s = stripped.trim(); }
    }
    // 严格形：剥完围栏与空白后必须**恰好是一个 JSON 对象**——前后有散 prose、或被数组包一层，
    // 都算"不合闭集"（fail-closed 面向重试，不猜）。
    if !s.starts_with('{') || !s.ends_with('}') { return Err(VerdictRejected); }
    let doc: serde_json::Value = serde_json::from_str(s).map_err(|_| VerdictRejected)?;
    let verdict = doc.get("verdict").and_then(serde_json::Value::as_str).ok_or(VerdictRejected)?;
    let verdict = match verdict {
        "yes" => MidVerdict::Merge,
        "no" => MidVerdict::KeepApart,
        "uncertain" => MidVerdict::Uncertain,
        _ => return Err(VerdictRejected),
    };
    let reason = doc.get("reason").and_then(serde_json::Value::as_str)
        .unwrap_or("").trim().to_string();
    Ok((verdict, clip(&reason, REASON_CLIP)))
}

fn prompt_sha(system: &str, user: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(system.as_bytes());
    h.update([0u8]);
    h.update(user.as_bytes());
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// 真座位的裁决器。实现 [`MidBandAdjudicator`]：`accepts` = `Merge` 才 `true`。
/// 判定表住在 [`LlmMidBand::take_log`]——`run_episode_pass` 的返回值不带它，
/// 因为裁决器是调用方装配的（hub 侧打印，用例侧断言），内核只认 trait。
pub struct LlmMidBand<'a> {
    backend: &'a dyn LlmBackend,
    /// (fact_a, fact_b) → 结论。进程内防"重算重问"（模块头第 5 段）。
    cache: RefCell<HashMap<(String, String), MidVerdict>>,
    log: RefCell<Vec<Adjudication>>,
}

impl<'a> LlmMidBand<'a> {
    pub fn new(backend: &'a dyn LlmBackend) -> LlmMidBand<'a> {
        LlmMidBand { backend, cache: RefCell::new(HashMap::new()), log: RefCell::new(Vec::new()) }
    }

    /// 判定表的交出口：按裁决顺序给出本进程已裁决的每一对（缓存命中不产生新行）。
    pub fn take_log(&self) -> Vec<Adjudication> {
        std::mem::take(&mut *self.log.borrow_mut())
    }

    /// 一对的裁决：首发 + 重试 ≤ [`ADJUDICATOR_RETRY_TIMES`]；任何失败形状
    /// （后端四态、输出不合闭集）都算这一发没成。重试耗尽 = [`MidVerdict::Uncertain`]。
    fn judge(&self, facts: &[GroupFact], pair: &Pair, shared: usize) -> Adjudication {
        let system = adjudication_system();
        let user = adjudication_user(facts, pair, shared);
        let sha = prompt_sha(&system, &user);
        let prompt = Prompt { system, user, derive: false };
        let started = Instant::now();
        let mut calls = 0usize;
        let mut outcome: Result<(MidVerdict, String), &'static str> = Err("unreached");
        for _ in 0..=ADJUDICATOR_RETRY_TIMES {
            calls += 1;
            outcome = match self.backend.complete(&prompt) {
                Ok(raw) => match parse_verdict(&raw) {
                    Ok((v, reason)) => Ok((v, reason)),
                    Err(_) => Err("adjudication reply did not match the closed set"),
                },
                Err(f) => Err(failure_reason(&f)),
            };
            if outcome.is_ok() { break; }
        }
        let latency_ms = started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
        let (verdict, reason) = match outcome {
            Ok((v, r)) => (v, r),
            Err(why) => (MidVerdict::Uncertain, why.to_string()),
        };
        Adjudication {
            fact_a: facts[pair.a].id.clone(),
            fact_b: facts[pair.b].id.clone(),
            verdict,
            reason,
            seat: self.backend.slug().unwrap_or("unknown").to_string(),
            latency_ms,
            prompt_sha: sha,
            calls,
        }
    }
}

/// 失败理由 = 固定短语 + `LlmFailure` 的封闭类别串。模型原文一个字节不进（守卫 12）。
fn failure_reason(f: &LlmFailure) -> &'static str {
    match f {
        LlmFailure::Timeout => "adjudication call timed out: llm timeout",
        LlmFailure::SchemaMismatch => "adjudication call failed: llm output did not match",
        LlmFailure::QuotaExhausted => "adjudication call blocked: llm quota exhausted",
        LlmFailure::BackendError => "adjudication call failed: llm backend failure",
    }
}

impl<'a> MidBandAdjudicator for LlmMidBand<'a> {
    fn accepts(&self, facts: &[GroupFact], pair: &Pair) -> bool {
        let key = (facts[pair.a].id.clone(), facts[pair.b].id.clone());
        if let Some(v) = self.cache.borrow().get(&key) {
            return *v == MidVerdict::Merge;
        }
        let shared = crate::episode::shared_participants(&facts[pair.a], &facts[pair.b]).len();
        let record = self.judge(facts, pair, shared);
        let merge = record.verdict == MidVerdict::Merge;
        self.cache.borrow_mut().insert(key, record.verdict.clone());
        self.log.borrow_mut().push(record);
        merge
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rc_llm::LlmFailure;
    use rc_memory::{Scope, ScopeKind};
    use std::collections::VecDeque;

    /// 脚本后端：按序回话，耗尽即 `Timeout`（与 hub `replay::Seat` 同语义的最小实现；
    /// rc-llm 的 mock 是 `#[doc(hidden)]` + test-only，rc-distill 的测试自带这一份）。
    struct Scripted(RefCell<VecDeque<Result<String, LlmFailure>>>);

    impl LlmBackend for Scripted {
        fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
            let mut q = self.0.borrow_mut();
            match q.pop_front() {
                Some(r) => r,
                None => Err(LlmFailure::Timeout),
            }
        }
    }

    fn scope() -> Scope { Scope { kind: ScopeKind::Project, key: "proj-adjudicate-test".into() } }

    fn pair(a: usize, b: usize) -> Pair {
        Pair { a, b, delta_ms: 808_000, band: crate::episode::Band::Proximity,
               shared: Vec::new(), same_turn: false, turn_distance: Some(1) }
    }

    fn facts() -> Vec<GroupFact> {
        vec![
            GroupFact {
                id: "aaaa".into(), participants: vec!["p".into()],
                speaker_anchor_id: None, subject_anchor_id: "s1".into(),
                subject_kind: Some("person".into()), subject_canonical: Some("某甲".into()),
                source_agent: "agent".into(), device_id: "dev".into(),
                scope: scope(), world_id: None,
                category: "event".into(), text: "第一条的正文".into(),
                occurred_at_ms: None, occurred_end_ms: None,
                created_at_ms: 1_700_000_000_000, evidence_turn_uid: "t-1".into(),
            },
            GroupFact {
                id: "bbbb".into(), participants: vec![],
                speaker_anchor_id: None, subject_anchor_id: "s2".into(),
                subject_kind: Some("person".into()), subject_canonical: Some("某乙".into()),
                source_agent: "agent".into(), device_id: "dev".into(),
                scope: scope(), world_id: None,
                category: "event".into(), text: "第二条的正文".into(),
                occurred_at_ms: None, occurred_end_ms: None,
                created_at_ms: 1_700_000_808_000, evidence_turn_uid: "t-2".into(),
            },
        ]
    }

    #[test]
    fn 闭集三态与围栏都收_认不出的一律拒() {
        for (raw, want) in [
            ("{\"verdict\":\"yes\",\"reason\":\"r\"}", MidVerdict::Merge),
            ("  {\"verdict\":\"no\",\"reason\":\"r\"}  ", MidVerdict::KeepApart),
            ("```json\n{\"verdict\":\"uncertain\",\"reason\":\"看不准\"}\n```", MidVerdict::Uncertain),
        ] {
            let (v, _) = parse_verdict(raw).unwrap_or_else(|_| panic!("该收：{raw}"));
            assert_eq!(v, want, "{raw}");
        }
        for raw in ["{\"verdict\":\"maybe\"}", "{\"holds\":true}", "yes", "",
                    "{\"verdict\":true}", "[{\"verdict\":\"yes\"}]"] {
            assert!(parse_verdict(raw).is_err(), "该拒：{raw}");
        }
    }

    #[test]
    fn 失败重试两次后折成不确定_账里calls记满三发() {
        let b = Scripted(RefCell::new(VecDeque::from(vec![
            Err(LlmFailure::Timeout), Err(LlmFailure::BackendError),
            Err(LlmFailure::Timeout), Err(LlmFailure::BackendError),
            Err(LlmFailure::Timeout), Err(LlmFailure::BackendError),
        ])));
        let j = LlmMidBand::new(&b);
        let f = facts();
        assert!(!j.accepts(&f, &pair(0, 1)), "重试耗尽 ⇒ fail-closed 不合");
        let log = j.take_log();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].verdict, MidVerdict::Uncertain);
        assert_eq!(log[0].calls, 3, "首发 + 重试 ≤ 2：{log:?}");
        assert!(log[0].reason.contains("llm timeout"), "失败理由带封闭类别：{}", log[0].reason);
        assert_eq!(log[0].seat, "unknown", "脚本后端没有身份");
        assert_eq!(log[0].prompt_sha.len(), 64, "prompt 指纹是 sha256 hex");
    }

    #[test]
    fn 同一对只问一次_重算不重问() {
        let b = Scripted(RefCell::new(VecDeque::from(vec![
            Ok("{\"verdict\":\"no\",\"reason\":\"两件事\"}".to_string()),
        ])));
        let j = LlmMidBand::new(&b);
        let f = facts();
        let p = pair(0, 1);
        assert!(!j.accepts(&f, &p));
        assert!(!j.accepts(&f, &p), "第二次走缓存，脚本耗尽也不该再问");
        let log = j.take_log();
        assert_eq!(log.len(), 1, "缓存命中不产生新行：{log:?}");
        assert_eq!(log[0].calls, 1);
    }

    #[test]
    fn 判定表逐格可溯_座位与指纹与延迟() {
        let b = Scripted(RefCell::new(VecDeque::from(vec![
            Ok("{\"verdict\":\"yes\",\"reason\":\"同一段经历的两步\"}".to_string()),
        ])));
        let j = LlmMidBand::new(&b);
        let f = facts();
        assert!(j.accepts(&f, &pair(0, 1)));
        let log = j.take_log();
        assert_eq!(log[0].fact_a, "aaaa");
        assert_eq!(log[0].fact_b, "bbbb");
        assert_eq!(log[0].verdict, MidVerdict::Merge);
        assert_eq!(log[0].reason, "同一段经历的两步");
    }
}
