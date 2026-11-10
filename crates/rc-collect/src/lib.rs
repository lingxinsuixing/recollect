//! `rc-collect`：本机采集器的配置、水位状态与 files 源扫描。
//!
//! 这个 crate 只做"发现新数据 → 切片 → 交给转换器 → 产出回执"，
//! 不碰数据库、不自己做时钟（时间一律由入参 `now_ms` 给）。

pub mod config;
pub mod lock;
pub mod run;
pub mod scan;
pub mod state;
