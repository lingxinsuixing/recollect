//! G17（计划 A8）：v3 → v1 下行投影。
//!
//! DSH 转换器产 exchange v3，而 `hub import` 只收 v1。裁定是接下行投影、不做废弃路线：
//! 本模块把 v3 的 session/turn 核心格（会话键、device、source_agent、turn 的
//! uid/role/body/时间戳/seq 指针等 v1 契约真有的格）投进 v1 [`ExchangeFile`]，
//! 然后 hub 把投影产物**原样喂给既有 v1 的 `parse`/`normalize`/`commit` 管线**——
//! 投影不产出任何绕过 v1 校验的行。
//!
//! v3 富字段在 v1 里没有家，一律丢弃并**逐字段计数**（[`V3Projection::dropped`]：
//! 字段名（点路径）→ 丢弃行数）。读数口径只有一个家，就在这里：
//!
//! - 会话层：恒在结构（`session_uid`/`ownership`/`source`/`environment.harness`/
//!   `environment.machine_id`）按会话数计；行数组（`threads`/`turns`/`participants`/
//!   `docs`/`secrets`/`roster.agents`/`roster.tools`）按行数计；Option 格只在值存在时计。
//!   `environment.machine` 投进 v1 的 `machine` 格，不计。
//! - 事件层：整行未投影的事件只计丢弃原因（`event.kind.<kind>` / `event.summary` /
//!   `event.final_` / `event.trust`（注入信任）/ `event.quarantine`），不另计它的字段；
//!   缺 `ts_ms` 的核心事件计 `event.ts_ms` 后丢弃（v1 的 turn 必须带时间戳）。
//!   已投影事件的富字段：Option/Vec/非默认值按事件数计；恒在标量
//!   （`event.thread_uid`/`event.native_key`/`event.content_hash`/`event.text_sha256`/
//!   `event.revision_id`/`event.norm_version`/`event.trust`/`event.speech_act`/
//!   `event.source_ref`/`event.speaker_id`）按事件数计。
//!
//! 失败语义（工单第 4 条）：读不成 v3 JSON → `JsonMalformed`（复用 validate 的同一个
//! 构造器）；读得成但 v3 闸不过 → `V3Invalid`（RejectCode 本工单新增的码）。
//! 两种都是整批拒收，由 hub 走既有 BatchReport/batch_reject 通道落审计行。

use crate::format::{ExchangeFile, RawSession, RawTurn, TurnTier, SUPPORTED_FORMAT_VERSIONS};
use crate::ingest_v3::{
    is_evidence_eligible, validate_ingest_v3, EventKind, Modality, Trust, V3Event, V3File,
    V3Session,
};
use crate::validate::{json_reject, Reject, RejectCode};
use std::collections::{BTreeMap, HashMap};

/// 投影产物：v1 文件 + 富字段丢弃读数。
#[derive(Debug)]
pub struct V3Projection {
    pub file: ExchangeFile,
    /// 丢弃读数：字段名（点路径）→ 丢弃行数。BTreeMap：JSON 面与文本面的渲染顺序都由它定死。
    pub dropped: BTreeMap<String, u64>,
}

/// 从原始字节投影 v3 → v1。失败返回 [`Reject`] 列表（整批拒收语义）。
pub fn project_v3_to_v1(bytes: &[u8]) -> Result<V3Projection, Vec<Reject>> {
    let file: V3File =
        serde_json::from_slice(bytes).map_err(|e| vec![json_reject(&e, bytes)])?;
    let report = validate_ingest_v3(&file);
    if !report.ok {
        // locator 到会话级：v3 的 issue.seq 是事件序号，与 v1 的 turn_index 不同层，
        // 不做跨层换算（换算就是发明第二种定位语义）。
        let index_of: HashMap<&str, usize> = file
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| (s.session_key.as_str(), i))
            .collect();
        return Err(report
            .errors
            .iter()
            .map(|issue| {
                let si = issue
                    .session
                    .as_deref()
                    .and_then(|key| index_of.get(key))
                    .copied()
                    .unwrap_or(0);
                // v3 的 issue message 只带身份值/计数/字段名（validate_ingest_v3 的写法），
                // 不带正文——与 Reject.detail 的守卫 12 同一口径。
                Reject::sess(
                    si,
                    RejectCode::V3Invalid,
                    format!("{}: {}", issue.code, issue.message),
                )
            })
            .collect());
    }

    let mut dropped: BTreeMap<String, u64> = BTreeMap::new();
    let sessions: Vec<RawSession> = file
        .sessions
        .iter()
        .map(|session| project_session(session, &mut dropped))
        .collect();
    Ok(V3Projection {
        file: ExchangeFile {
            format_version: SUPPORTED_FORMAT_VERSIONS[0],
            sessions,
        },
        dropped,
    })
}

fn bump(dropped: &mut BTreeMap<String, u64>, key: &str) {
    *dropped.entry(key.to_owned()).or_insert(0) += 1;
}

fn bump_n(dropped: &mut BTreeMap<String, u64>, key: &str, n: usize) {
    if n > 0 {
        *dropped.entry(key.to_owned()).or_insert(0) += n as u64;
    }
}

fn project_session(session: &V3Session, dropped: &mut BTreeMap<String, u64>) -> RawSession {
    bump(dropped, "session_uid");
    if session.parent_session_key.is_some() {
        bump(dropped, "parent_session_key");
    }
    if session.agent_instance_id.is_some() {
        bump(dropped, "agent_instance_id");
    }
    // environment：machine 投进 v1 的 `machine` 格；其余格 v1 没有家。
    bump(dropped, "environment.machine_id");
    bump(dropped, "environment.harness");
    for (name, value) in [
        ("environment.hostname", &session.environment.hostname),
        ("environment.os", &session.environment.os),
        ("environment.cwd", &session.environment.cwd),
        ("environment.git_branch", &session.environment.git_branch),
        ("environment.locale", &session.environment.locale),
        ("environment.timezone", &session.environment.timezone),
    ] {
        if value.is_some() {
            bump(dropped, name);
        }
    }
    bump_n(dropped, "roster.agents", session.roster.agents.len());
    bump_n(dropped, "roster.tools", session.roster.tools.len());
    bump(dropped, "ownership");
    bump_n(dropped, "threads", session.threads.len());
    bump_n(dropped, "turns", session.turns.len());
    bump_n(dropped, "participants", session.participants.len());
    bump_n(dropped, "docs", session.docs.len());
    bump_n(dropped, "secrets", session.secrets.len());
    if session.dropped.total() > 0 {
        bump(dropped, "dropped");
    }
    if !session.quality.flags.is_empty()
        || session.quality.quarantined_events > 0
        || session.quality.rejected_events > 0
        || session.quality.retryable
    {
        bump(dropped, "quality");
    }
    bump(dropped, "source");

    let turns: Vec<RawTurn> = session
        .events
        .iter()
        .filter_map(|event| project_event(event, dropped))
        .collect();

    RawSession {
        session_key: session.session_key.clone(),
        device_id: session.device_id.clone(),
        source_agent: session.source_agent.clone(),
        machine: session.environment.machine.clone(),
        // environment.cwd 是路径形状的现场值，v1 的 `project_hint` 只收逻辑名
        // （parse 的 ProjectHintNotLogical 会拒路径）。投影不替 v1 猜"哪个路径碰巧
        // 是逻辑名"——一律不投，cwd 的丢弃已按格计数。
        project_hint: None,
        last_saved_at_ms: None,
        owner_principal_id: None,
        principal_name: None,
        principal_source: None,
        turns,
    }
}

fn project_event(
    event: &V3Event,
    dropped: &mut BTreeMap<String, u64>,
) -> Option<RawTurn> {
    let role = match event.kind {
        EventKind::HumanMsg => "human",
        EventKind::AgentMsg => "agent",
        EventKind::System => "system",
        _ => {
            // 整行丢弃只计原因，不另计它的字段（一行一条读数）。
            bump(dropped, &format!("event.kind.{}", event.kind.as_str()));
            return None;
        }
    };
    // 证据资格判据只用 v3 自己的那一份（is_evidence_eligible）——投影不重写第二套。
    if !is_evidence_eligible(event) {
        let reason = if event.summary {
            "event.summary"
        } else if !event.final_ {
            "event.final_"
        } else if event.trust == Trust::Injected {
            "event.trust"
        } else {
            "event.quarantine"
        };
        bump(dropped, reason);
        return None;
    }
    // v1 的 turn 必须带 created_at_ms（parse 的 CreatedAtMissing）；
    // v3 里时间未知是合法状态，但 v1 没有它的家——丢行并计数。
    let Some(ts) = event.ts_ms else {
        bump(dropped, "event.ts_ms");
        return None;
    };

    if event.parent_seq.is_some() {
        bump(dropped, "event.parent_seq");
    }
    if event.observed_at_ms.is_some() {
        bump(dropped, "event.observed_at_ms");
    }
    if event.valid_from_ms.is_some() {
        bump(dropped, "event.valid_from_ms");
    }
    if event.valid_to_ms.is_some() {
        bump(dropped, "event.valid_to_ms");
    }
    if event.occurrence > 0 {
        bump(dropped, "event.occurrence");
    }
    if event.revision_of.is_some() {
        bump(dropped, "event.revision_of");
    }
    if event.duplicate_of.is_some() {
        bump(dropped, "event.duplicate_of");
    }
    if event.attempt > 0 {
        bump(dropped, "event.attempt");
    }
    if event.span.is_some() {
        bump(dropped, "event.span");
    }
    if !event.refs.is_empty() {
        bump(dropped, "event.refs");
    }
    if event.system_kind.is_some() {
        bump(dropped, "event.system_kind");
    }
    if !event.covers.is_empty() {
        bump(dropped, "event.covers");
    }
    if event.quality.is_some() {
        bump(dropped, "event.quality");
    }
    if event.modality != Modality::Actual {
        bump(dropped, "event.modality");
    }
    // 恒在标量：v1 没有这些格，投影行每行丢一次。
    for key in [
        "event.thread_uid",
        "event.turn_uid",
        "event.native_key",
        "event.content_hash",
        "event.text_sha256",
        "event.revision_id",
        "event.norm_version",
        "event.trust",
        "event.speech_act",
        "event.source_ref",
        "event.speaker_id",
    ] {
        bump(dropped, key);
    }

    Some(RawTurn {
        turn_uid: Some(event.event_uid.clone()),
        // seq 随行保留作指针；tier 恒 IdBearing（uid 是 v3 的稳定身份）。
        seq: Some(i64::from(event.seq)),
        tier: Some(TurnTier::IdBearing),
        role: role.to_owned(),
        body: event.text.clone(),
        created_at_ms: Some(ts),
        // 投影面最小（工单第 1 条）：speaker 不在核心格里，丢弃不计数外的处理。
        speaker: None,
    })
}
