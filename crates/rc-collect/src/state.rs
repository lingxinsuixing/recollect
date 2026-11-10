//! 采集水位状态 `collect-state.json`。
//!
//! 缺文件 = 空的默认状态（首次运行本来就没有水位）；文件存在但读不懂 = `Err`
//! （把坏文件猜成空会重采全部历史，甚至把 offset 归零后重复提交）。
//! 所有 map 用 `BTreeMap`，输出确定；写入走 tmp + rename，读者永远看不到半份 JSON。

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDoc {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, FileCursor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sqlite: BTreeMap<String, SqliteCursor>,
    /// 整文件源（`kind = "whole"`）的水位：键是源文件的绝对路径（正斜杠）。
    ///
    /// 它**不含字节偏移**：压缩流没有可切分的边界，文件变了就整份重转。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub whole: BTreeMap<String, WholeCursor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub daily: BTreeMap<String, DailyUse>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub poisoned: BTreeMap<String, Poison>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileCursor {
    pub mtime_ms: i64,
    pub size: i64,
    pub offset: i64,
    pub last_turns: i64,
    pub head_sha256: String,
}

/// 整文件源的水位。变更判定 = **三个字段全同 ⇒ 没变**（跳过）；任一不同 ⇒ 整份重转。
///
/// 没有 `offset` 这一格：整文件源不存在字节偏移的概念（恒为 0）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WholeCursor {
    pub mtime_ms: i64,
    pub size: i64,
    pub head_sha256: String,
    pub last_turns: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SqliteCursor {
    pub last_created_at_ms: i64,
    #[serde(default)]
    pub last_msg_id: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DailyUse {
    pub cap: u32,
    pub used: u32,
    /// 本窗口起点（毫秒）。0 = 旧状态文件，下一轮视为需要重置。
    #[serde(default)]
    pub start_ms: i64,
}

/// 预算键：日常增量（所有源共用）与历史回填（专用）各一条额度。
///
/// 两条预算是**两把独立的伞**：一次历史回填不该把当天的新会话额度吃光
/// （2026-11-10 实测：一次回填用掉 1,452 轮，额度到 11-11 才恢复）。
pub const BUDGET_WINDOW: &str = "window";
pub const BUDGET_BACKFILL: &str = "backfill";

/// 预算窗口长度：滚动 24 小时，不做日历日切分（无时区依赖）。
pub const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Poison {
    pub attempts: u32,
    pub last_code: String,
}

impl StateDoc {
    /// 读一条预算的当前用量。窗口过期（滚动 24 小时）或 cap 变了就归零。
    ///
    /// 返回 (用量, 状态是否被改动)：调用方据此决定要不要落盘。
    pub fn budget_used(&mut self, key: &str, cap: u32, now_ms: i64) -> (u32, bool) {
        match self.daily.get_mut(key) {
            Some(d) => {
                let expired = d.start_ms == 0 || now_ms.saturating_sub(d.start_ms) >= DAY_MS;
                if d.cap != cap || expired {
                    d.cap = cap;
                    d.used = 0;
                    d.start_ms = now_ms;
                    (0, true)
                } else {
                    (d.used, false)
                }
            }
            None => {
                self.daily.insert(
                    key.to_string(),
                    DailyUse {
                        cap,
                        used: 0,
                        start_ms: now_ms,
                    },
                );
                (0, true)
            }
        }
    }

    /// 记一次消耗。窗口新鲜性由 `budget_used` 负责，这里只加数。
    pub fn budget_spend(&mut self, key: &str, turns: u32) {
        if let Some(d) = self.daily.get_mut(key) {
            d.used = d.used.saturating_add(turns);
        }
    }

    /// 缺文件 ⇒ 默认空；JSON 坏 ⇒ `Err`（不猜空）。
    pub fn load(path: &Path) -> Result<StateDoc, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(StateDoc::default()),
            Err(_) => return Err("collect-state.json is unreadable".to_string()),
        };
        serde_json::from_str(&text).map_err(|_| "collect-state.json is unreadable".to_string())
    }

    /// tmp + rename + 单个 `\n`；renames 覆盖原文件是原子的。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let mut body = serde_json::to_string(self)
            .map_err(|_| "collect-state.json is not serializable".to_string())?;
        body.push('\n');
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|_| "collect-state.json is not writable".to_string())?;
            }
        }
        let name = path
            .file_name()
            .ok_or_else(|| "collect-state.json is not writable".to_string())?;
        let tmp = path.with_file_name(format!(
            "{}.tmp-{}",
            name.to_string_lossy(),
            std::process::id()
        ));
        if std::fs::write(&tmp, body.as_bytes()).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err("collect-state.json is not writable".to_string());
        }
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return Err("collect-state.json is not writable".to_string());
        }
        Ok(())
    }
}
