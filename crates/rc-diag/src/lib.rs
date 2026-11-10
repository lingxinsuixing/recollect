//! 诊断面：能力与平台自检 + doctor 视图。**零正文、只计数**。
//!
//! 这一层**不取时钟**（库 crate 一律不许取钟）："距上次成功备份 N 小时"里的
//! "现在"由调用方传进来。

pub mod connectivity;
pub mod doctor;
