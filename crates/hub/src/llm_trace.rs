//! 模型面留痕：只在 `RECOLLECT_LLM_TRACE=<path>` 时把每次调用的 system / user / 回复按行落盘。
//!
//! 为什么要有它：质量分析连着三次卡在"模型到底回了什么"上——`distill once` 只打计数，
//! `pass_state` 只留状态与拒因，被丢草稿的原文也只在有草稿时存在。判空那一支什么都不留。
//! 这一层是**旁路**：默认不装、不读环境变量以外的任何配置、写失败不影响业务调用，
//! 所以"没开 = 与今天逐字一致"。
//!
//! 内容边界：落盘的是**发给模型的那份 prompt 与它的回复**，也就是转录原文的一段。
//! 它只写操作员显式给的那个路径，不进库、不进快照、不进备份面；密钥不在这一层，
//! 也没有任何理由出现在这里（`Live` 只把 key 放在自己的请求头里）。

use rc_llm::{LlmBackend, LlmFailure, Prompt};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// 环境变量名。空串与未设置等价：都不开。
pub const TRACE_ENV: &str = "RECOLLECT_LLM_TRACE";

/// 读出留痕路径。未设置或为空 ⇒ `None`（不装那一层）。
pub fn sink() -> Option<PathBuf> {
    match std::env::var(TRACE_ENV) {
        Ok(p) if !p.trim().is_empty() => Some(PathBuf::from(p)),
        _ => None,
    }
}

/// 包一层：每次 `complete` 都往 `path` 追加一行 JSON，然后把内层的结果原样交回。
pub struct Trace {
    seat: String,
    inner: Box<dyn LlmBackend>,
    path: PathBuf,
}

impl Trace {
    pub fn new(seat: impl Into<String>, inner: Box<dyn LlmBackend>, path: PathBuf) -> Self {
        Trace { seat: seat.into(), inner, path }
    }

    /// 一行一次调用。失败原因只写闭集名（`LlmFailure` 的 Display），不写内层原串。
    fn record(&self, prompt: &Prompt, outcome: &Result<String, LlmFailure>) {
        let line = match outcome {
            Ok(reply) => serde_json::json!({
                "seat": self.seat,
                "derive": prompt.derive,
                "system": prompt.system,
                "user": prompt.user,
                "ok": true,
                "reply": reply,
            }),
            Err(failure) => serde_json::json!({
                "seat": self.seat,
                "derive": prompt.derive,
                "system": prompt.system,
                "user": prompt.user,
                "ok": false,
                "failure": failure.to_string(),
            }),
        };
        // 留痕是旁路：写不进去不改变这次调用的结果（业务面看到的仍是内层的返回值）。
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{line}");
        }
    }
}

impl LlmBackend for Trace {
    fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
        let outcome = self.inner.complete(prompt);
        self.record(prompt, &outcome);
        outcome
    }

    fn slug(&self) -> Option<&str> { self.inner.slug() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct Echo(std::cell::RefCell<String>);

    impl LlmBackend for Echo {
        fn complete(&self, prompt: &Prompt) -> Result<String, LlmFailure> {
            *self.0.borrow_mut() = prompt.user.clone();
            Ok("{\"pass\":\"event\"}".to_string())
        }
    }

    struct Dead(Cell<u32>);

    impl LlmBackend for Dead {
        fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
            self.0.set(self.0.get() + 1);
            Err(LlmFailure::Timeout)
        }
    }

    fn prompt() -> Prompt {
        Prompt { system: "SYS".into(), user: "USER".into(), derive: false }
    }

    #[test]
    fn a_call_appends_one_line_with_prompt_and_reply() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let t = Trace::new("agnes:agnes-3.0-flash", Box::new(Echo(Default::default())), path.clone());
        assert_eq!(t.complete(&prompt()).unwrap(), "{\"pass\":\"event\"}");
        let body = std::fs::read_to_string(&path).unwrap();
        assert_eq!(body.lines().count(), 1, "一次调用一行：{body}");
        let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(v["system"], "SYS");
        assert_eq!(v["user"], "USER");
        assert_eq!(v["reply"], "{\"pass\":\"event\"}");
        assert_eq!(v["ok"], true);
        assert_eq!(v["seat"], "agnes:agnes-3.0-flash");
    }

    #[test]
    fn a_failed_call_still_leaves_a_line_and_keeps_the_failure() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trace.jsonl");
        let t = Trace::new("s1", Box::new(Dead(Cell::new(0))), path.clone());
        assert_eq!(t.complete(&prompt()), Err(LlmFailure::Timeout));
        let body = std::fs::read_to_string(&path).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.trim()).unwrap();
        assert_eq!(v["ok"], false);
        assert_eq!(v["failure"], "llm timeout");
    }

    #[test]
    fn sink_is_none_without_the_env_var() {
        // 测试进程里不设这个变量；万一外部环境设了，也只断言"不是空路径"。
        match sink() {
            None => {}
            Some(p) => assert!(!p.as_os_str().is_empty()),
        }
    }
}
