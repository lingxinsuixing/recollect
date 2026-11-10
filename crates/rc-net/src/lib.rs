//! 协议与传输层。**不认任何存储句柄**：中心侧的数据库动作全部经 `Handler` 注入。
//! 这一条是守卫 1 的边界——`rc-net` 一旦把一个存储句柄类型写进签名，它就变成第二个能同时命名三柄的 crate。

pub mod discovery;
pub mod error;
pub mod http;
pub mod hub;
pub mod ingest;
pub mod invite;
pub mod key;
pub mod node;
pub mod pair;
pub mod protocol;
pub mod snapshot;
pub mod tls;
