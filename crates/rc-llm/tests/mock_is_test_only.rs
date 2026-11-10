use rc_llm::{LlmBackend, LlmFailure, Prompt, mock::MockBackend};

#[test]
fn scripted_replies_are_consumed_in_order_then_timeout() {
    let b = MockBackend::scripted(vec![Ok("A".into()), Err(LlmFailure::Timeout), Ok("C".into())]);
    let p = || Prompt { system: "s".into(), user: "u".into(), derive: false };
    assert_eq!(b.complete(&p()).unwrap(), "A");
    assert!(matches!(b.complete(&p()), Err(LlmFailure::Timeout)));
    assert_eq!(b.complete(&p()).unwrap(), "C");
    // 脚本耗尽 ⇒ Timeout，**绝不返回 Ok("")**：空回复会被下游读成"库里没有新东西"（假空）。
    assert!(matches!(b.complete(&p()), Err(LlmFailure::Timeout)));
    assert_eq!(b.calls(), 4);
}
