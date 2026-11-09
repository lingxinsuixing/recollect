//! hub 的库面：只为"集成测试能直接构造 HubState 并调 Handler"而存在。
//! `main.rs` 的 replay/作业编排仍是 bin 私有——它们不该被外部测试当成 API。

pub mod backup;
pub mod hub_cfg;
pub mod ingest;
pub mod key_archive;
pub mod llm_trace;
pub mod ownership;
/// W37-A2 / G13④：存量缺章记忆行的带账回填（`hub principal backfill`，默认 dry-run）。
pub mod principal_backfill;
/// W25：抽干前的水位闸读数（`hub pending`）。只读，调用方用 `Root::open_readonly` 装配。
pub mod pending;
pub mod promote;
pub mod purge;
pub mod reconcile;
pub mod replicate;
pub mod review;
pub mod restore;
pub mod rows;
pub mod serve;
pub mod snapshot;
/// W39 刀 C：中心转录只读路由（`/v1/transcript/*`，复用 rc-search 的唯一读闸）。
pub mod transcript_read;
