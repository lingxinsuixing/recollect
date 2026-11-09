//! L0 文本与它的版本戳。
//!
//! `L0_TEXT` 用 `include_str!` 在编译期把仓库里的 `skills/recollect-L0.md` 装进产物：
//! 文本只有一份，二进制里的那份不会是"上次构建的旧拷贝"。
//! `L0_SHA256` 是**文本文件字节**的小写 sha256，`tests/l0.rs` 读文件现算并与之对拍——
//! 改文本不改戳，`cargo test -p rc-agent --test l0` 与阶段 5 判据 4⑧ 会红。
//! 改文本时必须同步更新这里（先改文本，再 `sha256sum` 现算填入）。

/// 给 agent 的使用说明，与仓库里的 `skills/recollect-L0.md` 在编译期同源。
pub const L0_TEXT: &str = include_str!("../../../skills/recollect-L0.md");

/// `skills/recollect-L0.md` 的小写十六进制 sha256。改文本必须同时改这里；tests/l0.rs 会对拍。
pub const L0_SHA256: &str = "ab8e6d40bede69fac57685592cc6bda0c301cd818b3640f2e3de18ad7b9abcbb";
