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

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Poison {
    pub attempts: u32,
    pub last_code: String,
}

impl StateDoc {
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
