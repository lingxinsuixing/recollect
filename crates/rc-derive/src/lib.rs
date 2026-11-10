//! 推导层（架构 §7，阶段 3 唯一新增 crate）。
//!
//! ## 守卫 1：拿不到会话/账句柄
//!
//! 本 crate 的依赖里没有 `rc-ledger`；任何"想写一句 `use rc_ledger::...` 来读
//! `LedgerHandle`/`TranscriptHandle`"的尝试都被编译期拒绝。
//! 源文本断言 `tests/guard1.rs` 再挡一道（`Cargo.toml` 与 `src/*.rs` 都不许出现
//! `LedgerHandle` / `TranscriptHandle` / `rc_ledger`，也不许出现 `Root::open` /
//! `.ledger()` / `.transcript()` 这三条绕道）。
//! **这道守卫执法的是三件事：依赖图（`Cargo.toml` 不列 `rc-ledger`）、入口签名（只收
//! `&MemoryHandle`）、源文本断言（`tests/guard1.rs`）——不是类型层的不可能性**：句柄类型住在
//! `rc-store`（本 crate 的公开依赖），`Root::open` 与 `Root::ledger()` 都是 `pub`，
//! 所以类型层拦不住"自己开一个 Root 再取账柄"，那条只能由源文本断言拦。
//! 文本网是**子串网，不追改名**：`use rc_store::Root as S;` + `S::open(d)?` + `S::ledger(&r)`
//! 这一支（别名导入 + UFCS 调用）六个禁符一个都不出现，`guard1` 照绿——那一格只剩
//! 依赖图、入口签名与评审三道兜着，不是文本层。
//! 真正由类型保证的是另一件事：本 crate 的入口只收 `&MemoryHandle`，推导作业拿不到
//! 柄就读不到会话行——把会话行当成记忆行、读 transcript 库的列，都得先越过 `guard1`。
//!
//! ## 边界
//!
//! 入口 = `select_batch`（批选择，调 `rc_memory::reader::rows_since`）；
//! 出口 = `derive_pass`（T4 落，prompt → backend → parse → 落库）。
//! 两入口的输入都是 `&MemoryHandle`；后端的 `&dyn LlmBackend` 是 T3 引入。
//! 编排函数 `rc_distill::derive::run_derive` 住 `rc-distill`（它已持双柄），
//! `rc-distill/Cargo.toml` 加本 crate 依赖。
//!
//! ## 阶段 3 写
//!
//! T3 prompt + parse、T4 derive_pass + write_edge；T6 连带抑制扫描；
//! T7 两跳溯源读侧。T5 接线。
pub mod select;
pub mod parse;
pub mod prompt;
pub mod derive;
pub mod date;
