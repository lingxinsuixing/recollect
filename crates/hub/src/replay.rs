//! 回放后端（§2.16）：操作员递一个 JSON 数组文件，hub 按序把它当成 LLM 的回复。
//!
//! 它住 hub 不住 `rc-llm`，两条理由都是硬的：① `rc-llm` 的依赖清单是**空的**，解析 JSON 要
//! `serde_json`，加进去等于把"LLM 面拿不到任何句柄"改成"LLM 面有一个能读文件的实现"；
//! ② 它与 rc-llm 那个 `#[doc(hidden)]` 脚本后端是同一件事的两份实现（按序消费脚本、耗尽即 `Timeout`），破守卫 3。
//! 文件解析因此是**宿主侧**的事，跟 `import` 那条一样由 hub 承担。
//!
//! 这一份文件今天只剩两件事（B2b-1 起）：
//! - [`Seat`] = **来源**：一份脚本，按序回答，落账一律过它借用的那本 [`Metered`]；
//! - [`load`] / [`load_replies`] = **读文件**：把 `--replies` 解析成脚本（以及第 1 座的那本账）。
//!
//! **账本**（`Metered`）已经搬进 `rc-cycle`：节点离线自蒸要与中心共用同一种账本形状，而
//! "这一轮烧了几次口"本来就是管线的账面，不是回放脚本的属性。座位仍是 hub 的事——`llm.toml`
//! 那些座位（`Live`）也要有人替它们记账，而账本的形状不该长在脚本上。
//! §4.8 ③ 段 B 之后递进 `run_cycle_once` 的确实是 `Rotating`，但它包住的每一座仍是这里的
//! [`Seat`]，落的还是账本那两个口（`record_call` / `mark_quota_exhausted`）——账本一行没改，
//! 两座共享同一本（裁定 D③），于是 P-33 那条算术的口径一字不动。

use rc_cycle::Metered;
use rc_llm::{LlmBackend, LlmFailure, Prompt};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::Path;
use std::rc::Rc;

/// 一次回复的两种形状：回复原文，或者四态之一。
pub(crate) enum Reply {
    Text(String),
    Fail(LlmFailure),
}

/// `{"fail":"<slug>"}` 的四态。这份词表与 `LlmFailure` 的四个变体一一对应，
/// 所以它是 `match` 而不是查表，且**认不出的一律 `None`** ⇒ 由调用方落进同一条类别串。
fn failure_slug(s: &str) -> Option<LlmFailure> {
    match s {
        "timeout" => Some(LlmFailure::Timeout),
        "schema_mismatch" => Some(LlmFailure::SchemaMismatch),
        "quota_exhausted" => Some(LlmFailure::QuotaExhausted),
        "backend_error" => Some(LlmFailure::BackendError),
        _ => None,
    }
}

/// 四种读侧失败（读不到 / 顶层不是数组 / 元素形状不对 / `fail` slug 认不出）共用这一条串：
/// 拼错一个词与递错一个文件是同一类操作员错误（P-06 同族），而运维面只许有类别（守卫 12）。
const BAD_REPLIES: &str = "cannot parse --replies file";

/// CLI 路径的账本构造点。账本本体住 `rc-cycle`（`Metered::new`），不收任何上限：一轮问几次
/// 由 `rc-distill` 的批大小决定，这一层只给它一本空账。
///
/// 返回的是**元组**（账本 + 脚本），不是 `Metered` 自己：那一份脚本字节归 [`Seat`]，
/// 这一层因此读不出"回放"这件事，只读得出"这轮烧了几次口"。
/// 四条读侧失败仍然一律折成 `BAD_REPLIES` 那一条类别（守卫 12 在拆分前后同形）。
pub fn load(path: &Path) -> Result<(Metered, Vec<Reply>), &'static str> {
    Ok((Metered::new(), load_replies(path)?))
}

/// 一份脚本，**不造账本**。§4.8 ③ 段 B 的裁定 D③ 要两座共享同一本 `Metered`
/// （新开一本就是给同一个事实两份表示，`charge_counters` 的口径会静默变成"只数第一座"），
/// 所以第二座之后的座位只解析脚本、不再各自带一本账。
/// 四条读侧失败的类别串与 `load` 同一条：座位序号不进运维面（守卫 12）。
pub fn load_replies(path: &Path) -> Result<Vec<Reply>, &'static str> {
    let bytes = std::fs::read(path).map_err(|_| BAD_REPLIES)?;
    let doc: Value = serde_json::from_slice(&bytes).map_err(|_| BAD_REPLIES)?;
    let arr = doc.as_array().ok_or(BAD_REPLIES)?;
    let mut replies = Vec::with_capacity(arr.len());
    for item in arr {
        match item {
            Value::String(s) => replies.push(Reply::Text(s.clone())),
            Value::Object(o) => {
                let slug = o.get("fail").and_then(Value::as_str).ok_or(BAD_REPLIES)?;
                replies.push(Reply::Fail(failure_slug(slug).ok_or(BAD_REPLIES)?));
            }
            _ => return Err(BAD_REPLIES),
        }
    }
    Ok(replies)
}

/// 来源（§4.8 ② 拆分的后一半）：一份脚本 + 它替谁记账。
///
/// 账本句柄是 `Rc` 而不是 `&'a Metered`（§4.8 ③ 段 B 换的，理由不在 hub 而在 `rc-llm`）：
/// `Rotating` 的槽位是 `Box<dyn LlmBackend>`，那个裸 trait 对象自带 `+ 'static`，
/// 于是"借账本的引用"这一形根本进不去（E0310），而 `Rotating` 与它的签名是 Task 6 封板的层，
/// 本段明令不许动。共享语义因此换了一种写法但**没有变弱**：`Rc` 只有一个值，
/// 两座拿的是同一个句柄 ⇒ "同一本账的两个角色"仍是结构事实，不是注释。
/// 单线程 ⇒ `Rc` 够用；这也与 `rc-llm` 自己的轮转测试怎么装槽位同形。
pub struct Seat {
    script: RefCell<VecDeque<Reply>>,
    ledger: Rc<Metered>,
}

impl Seat {
    pub(crate) fn new(replies: Vec<Reply>, ledger: Rc<Metered>) -> Seat {
        Seat { script: RefCell::new(replies.into()), ledger }
    }
}

impl LlmBackend for Seat {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        let next = self.script.borrow_mut().pop_front();
        self.ledger.record_call();
        match next {
            Some(Reply::Text(s)) => Ok(s),
            Some(Reply::Fail(LlmFailure::QuotaExhausted)) => {
                self.ledger.mark_quota_exhausted();
                Err(LlmFailure::QuotaExhausted)
            }
            // 观测面（W33-A10）：脚本**显式声明**的超时记一笔。脚本耗尽那一支（`None`）不记——
            // 它交回的 `Timeout` 是"操作员没给够"的形状（见下面那支注释），不是传输超时；
            // 把它记进 `timeouts` 会让一次喂脚本的手误在观测面上冒充一次网络事故。
            Some(Reply::Fail(LlmFailure::Timeout)) => {
                self.ledger.note_timeout();
                Err(LlmFailure::Timeout)
            }
            Some(Reply::Fail(f)) => Err(f),
            // 脚本耗尽：与 rc-llm 那个 `#[doc(hidden)]` 脚本后端同一条语义。它报的是"操作员没给够"，
            // 而 hub 把它当运维失败（3），于是"忘了再喂一条"读起来是 3 而不是 2。
            None => Err(LlmFailure::Timeout),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seat_counts_every_call_and_shuts_nothing() {
        // 拆分之后这一条是**接线**用例：计数住在账本，脚本住在来源，`Seat` 不落账 ⇒ 红。
        //
        // 2026-11-06 用户裁定"调用配额是预算，砍掉"：这条用例的前身钉的是"第 N 发被本地入口
        // 闸门拦下、被拦的那一次不算调用"。本地闸门已经不存在，于是同一串调用必须一路走通、
        // 一次不少地进账——把闸加回去，这一条立刻红。
        // `mut` 不留：`Metered` 与 `Seat` 的写全走 `Cell`/`RefCell` 的 `&self`，`let mut` 会被
        // `unused_mut` 判红（本机实测）。
        let m = Rc::new(Metered::new());
        let seat = Seat::new(vec![Reply::Text("a".into()), Reply::Text("b".into())], m.clone());
        m.use_family(rc_llm::JobFamily::Extract);
        let p = || Prompt { system: "s".into(), user: "u".into(), derive: false };
        assert_eq!(seat.complete(&p()).unwrap(), "a");
        assert_eq!(seat.complete(&p()).unwrap(), "b");
        assert_eq!(m.calls(rc_llm::JobFamily::Extract), 2, "开口两次就进账两次");
        assert!(!m.saw_quota_exhausted(), "答得出话的一轮不是额度事件");
    }

    #[test]
    fn an_exhausted_script_is_a_timeout_counted_as_a_call() {
        // 拆分唯一可能改坏的那一条语义：脚本耗尽 ⇒ `Timeout`，**且那一次照样算调用**
        // （它真的开过口，只是没词了）。`cycle_once.rs` 的 `llm_calls_total` 算术靠这一条兜住
        // 来源那一侧，这一条兜住拆分本身：把 `None` 那一支的 `record_call()` 挪到 `match` 之后
        // 或整支删掉，本条当场红（`calls` 少一格 / 拿到别的失败态）。
        let m = Rc::new(Metered::new());
        let seat = Seat::new(vec![Reply::Text("a".into())], m.clone());
        m.use_family(rc_llm::JobFamily::Audit);
        let p = || Prompt { system: "s".into(), user: "u".into(), derive: false };
        assert_eq!(seat.complete(&p()).unwrap(), "a");
        assert_eq!(seat.complete(&p()).unwrap_err(), LlmFailure::Timeout, "耗尽报的是 Timeout，不是别的态");
        assert_eq!(m.calls(rc_llm::JobFamily::Audit), 2, "耗尽那一次也开过口：P-33 的算术把它算进去");
        assert!(!m.saw_quota_exhausted(), "Timeout 不是配额事件：这一格不许被点亮");
    }
}
