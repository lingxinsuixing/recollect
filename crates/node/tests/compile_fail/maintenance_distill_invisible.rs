// 架构 §3 权限侧②：远程侧没有一条路径能到达维护作业。
// **故意越界**：node 一旦依赖 rc-distill，这张夹具会编译通过，trybuild 当场变红。
use rc_distill as _;

fn main() {}
