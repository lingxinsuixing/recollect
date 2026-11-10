// 守卫 6 的类型层那一半：消费侧的装配结果上，"会话库句柄"这个方法不存在。
// 它比"没调用"强——没调用可以明天就有，方法不存在要改的是 rc-store。
use rc_store::AgentRoot;
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let ar = AgentRoot::open(std::path::Path::new("."))?;
    let _t = ar.transcript();
    Ok(())
}
