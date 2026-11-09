// 架构 §3 权限侧②的同一条边界：推导入口同样不在远程侧的作用域里。
// **故意越界**：node 一旦依赖 rc-derive，这张夹具会编译通过，trybuild 当场变红。
use rc_derive as _;

fn main() {}
