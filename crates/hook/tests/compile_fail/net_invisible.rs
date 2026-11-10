// 守卫 6 的编译期那一半：网络面在 `hook` 的作用域里**不存在**。
// 不写任何 API 调用——crate 名解析失败与函数签名无关，一张空 main 就够，
// 而写了假常量会在未来某天（真依赖进来时）报一条与守卫无关的错误。
use rc_net::Transport;
// 本 crate 现在还不存在（`rc-net` 属阶段 2/3），所以这张 fixture **今天是恒真的**：
// 任何 crate 里都编不过。它保护的是将来——阶段 2/3 把 `rc-net` 建起来、而某次改
// `crates/hook/Cargo.toml` 手滑把它挂上（**含 dev-dependencies**，trybuild 编 fixture 时
// 看得见那一片）时，`expected compile-fail, but it passed` 会当场红。

fn main() {}
