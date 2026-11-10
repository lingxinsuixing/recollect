use rc_agent::{ToolError, TurnSink};

struct S;
impl TurnSink for S {
    fn append_agent_turn(&self, _k: &str, _b: &str, _t: i64) -> Result<String, ToolError> {
        Ok("a".repeat(64))
    }
}

fn main() {
    let s = S;
    // 守卫 5 / 任务 8 开头 (2) 的越界尝试：把"谁在说话"交回调用方。
    let _ = s.append_agent_turn("sess", "正文", "human", 0);
}
