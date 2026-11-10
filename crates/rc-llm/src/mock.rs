//! 测试夹具工厂：`#[doc(hidden)]`，**MockBackend 只许测试引用**——生产代码引用它就是任务 13
//! 判据"mock 只许住在 tests"要红的事（canary 纪律收据，与 `rc-memory/src/synth.rs` 的 12b/12c 同一形制）。
//! 纪律同 `rc-memory/src/synth.rs`：① 内容全是编造的短句；② 不裸写 SQL、不碰句柄（这里根本没有句柄）。

use crate::{LlmBackend, LlmFailure, Prompt};
use std::cell::RefCell;

pub struct MockBackend {
    replies: RefCell<std::vec::IntoIter<Result<String, LlmFailure>>>,
    calls: RefCell<usize>,
}

impl MockBackend {
    pub fn scripted(replies: Vec<Result<String, LlmFailure>>) -> MockBackend {
        MockBackend { replies: RefCell::new(replies.into_iter()), calls: RefCell::new(0) }
    }
    pub fn calls(&self) -> usize {
        *self.calls.borrow()
    }
}

impl LlmBackend for MockBackend {
    fn complete(&self, _prompt: &Prompt) -> Result<String, LlmFailure> {
        *self.calls.borrow_mut() += 1;
        // 耗尽 ⇒ Timeout，绝不返回 Ok(空串)：那会被下游读成"库里没有新东西"（假空，守卫 8 一族）。
        self.replies.borrow_mut().next().unwrap_or(Err(LlmFailure::Timeout))
    }
}
