//! NDJSON 行拆解与 gzip 解压（T2）。**零存储依赖**：这里只产出"能交给
//! rc-contract 的 normalize 的会话"与"逐行拒收"，落库在 hub 的 ingest 模块。
//!
//! 一条行的契约：一个自足的 `ExchangeFile`，`sessions` 恰好一条。一条会话只许出现在
//! 一行里——同一 `session_key` 第二次出现是 `duplicate_session`。拆行会让一个会话的
//! seq/tier 判据在两边各看不全。

use crate::protocol::MAX_BODY_BYTES;
use rc_contract::{safe_token, ExchangeFile};
use serde::Serialize;
use std::io::Read;

/// 一次响应最多回多少条逐行拒收。
pub const MAX_REPORTED_REJECTS: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecodeError {
    UnsupportedEncoding,
    Corrupt,
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LineReject {
    /// 1 起。
    pub line: usize,
    /// 整行被拒时为 `None`；行被接受、但其中某条 turn 被 normalize 拒时是 turn 下标。
    pub turn: Option<usize>,
    /// 封闭码：`line_malformed` / `device_mismatch` / `duplicate_session`，
    /// 或 rc-contract 的 `RejectCode::slug()`（同一套审计词表）。
    pub code: String,
    /// 只许字段名与形状安全的 token（守卫 12）。
    pub detail: String,
    /// 整行被拒时这一行里**已知**的 turn 数（解析不出来的行是 0）；
    /// 逐条 turn 拒收时是 0——那一条由 `BatchReport::rejected_turn_count` 数。
    pub turns: usize,
}

#[derive(Debug)]
pub struct LineBatch {
    /// 只含通过逐行检查的会话；`format_version` 取自最后一条被接受的行。
    pub file: ExchangeFile,
    /// `file.sessions[i]` 来自第几行（1 起）。
    pub line_of_session: Vec<usize>,
    /// 与 `file.sessions` 逐位对齐的设备 id（拒收归因要用）。
    pub devices: Vec<String>,
    pub rejects: Vec<LineReject>,
    pub lines_in: usize,
    pub lines_accepted: usize,
}

impl LineBatch {
    /// 整行被拒、因此没进 `BatchReport` 的 turn 数。
    pub fn rejected_turns(&self) -> usize {
        self.rejects
            .iter()
            .filter(|r| r.turn.is_none())
            .map(|r| r.turns)
            .sum()
    }

    /// 上面那个数里"设备不符"的那一部分。它是子集、不是并列项：
    /// 总数已经含了它，两个计数器**不许相加**。
    pub fn signature_rejected_turns(&self) -> usize {
        self.rejects
            .iter()
            .filter(|r| r.turn.is_none() && r.code == "device_mismatch")
            .map(|r| r.turns)
            .sum()
    }
}

pub fn decode_body(encoding: Option<&str>, body: &[u8]) -> Result<Vec<u8>, DecodeError> {
    decode_body_limited(encoding, body, MAX_BODY_BYTES)
}

/// `max` 是**解压后**的字节上限。拆出来只为让"压缩炸弹"那一格能在几十字节上跑红，
/// 不必在测试里造一个 64 MiB 的展开。
pub fn decode_body_limited(
    encoding: Option<&str>,
    body: &[u8],
    max: usize,
) -> Result<Vec<u8>, DecodeError> {
    let enc = encoding.map(str::trim).unwrap_or("");
    if enc.is_empty() || enc.eq_ignore_ascii_case("identity") {
        if body.len() > max {
            return Err(DecodeError::TooLarge);
        }
        return Ok(body.to_vec());
    }
    if !enc.eq_ignore_ascii_case("gzip") {
        return Err(DecodeError::UnsupportedEncoding);
    }
    // 边解边限：先解完再量，量之前内存已经被吃掉了。
    let mut out = Vec::new();
    let n = flate2::read::GzDecoder::new(body)
        .take(max as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|_| DecodeError::Corrupt)?;
    if n > max {
        return Err(DecodeError::TooLarge);
    }
    Ok(out)
}

pub fn assemble(body: &[u8], device_id: &str) -> LineBatch {
    let mut batch = LineBatch {
        file: ExchangeFile {
            format_version: rc_contract::SUPPORTED_FORMAT_VERSIONS[0],
            sessions: Vec::new(),
        },
        line_of_session: Vec::new(),
        devices: Vec::new(),
        rejects: Vec::new(),
        lines_in: 0,
        lines_accepted: 0,
    };
    let mut seen: std::collections::HashSet<String> = Default::default();
    for (i, raw) in body.split(|b| *b == b'\n').enumerate() {
        let line_no = i + 1;
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        // 空行与纯空白行不是数据：把末尾换行读成一条拒收会让每一批都带一条假错误。
        if line.iter().all(|b| b.is_ascii_whitespace()) {
            continue;
        }
        batch.lines_in += 1;
        let file = match rc_contract::parse(line) {
            Ok(f) => f,
            Err(rejects) => {
                // 同一行可能同时踩中两个分型（缺 created_at 且正文为空）：逐条回，行号相同。
                for r in rejects {
                    batch.rejects.push(LineReject {
                        line: line_no,
                        turn: None,
                        code: r.code.slug(),
                        detail: r.detail,
                        turns: 0,
                    });
                }
                continue;
            }
        };
        if file.sessions.len() != 1 {
            batch.rejects.push(LineReject {
                line: line_no,
                turn: None,
                code: "line_malformed".into(),
                detail: "sessions".into(),
                turns: file.sessions.iter().map(|s| s.turns.len()).sum(),
            });
            continue;
        }
        let session = file.sessions.into_iter().next().expect("sessions.len() == 1");
        if session.device_id != device_id {
            // 声明不归签名人时整行拒收：这不是"替别人导入"，而是"这批数据不属于你"。
            batch.rejects.push(LineReject {
                line: line_no,
                turn: None,
                code: "device_mismatch".into(),
                detail: "device_id".into(),
                turns: session.turns.len(),
            });
            continue;
        }
        if !seen.insert(session.session_key.clone()) {
            batch.rejects.push(LineReject {
                line: line_no,
                turn: None,
                code: "duplicate_session".into(),
                // session_key 是生产者可控文本（可能是路径或正文的一段）⇒ 过同一个形状判据。
                detail: safe_token(&session.session_key).to_string(),
                turns: session.turns.len(),
            });
            continue;
        }
        batch.file.format_version = file.format_version;
        batch.file.sessions.push(session);
        batch.line_of_session.push(line_no);
        batch.devices.push(device_id.to_string());
        batch.lines_accepted += 1;
    }
    batch
}
