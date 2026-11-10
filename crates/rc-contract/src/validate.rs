use crate::format::{ExchangeFile, RawTurn, TurnTier, SUPPORTED_FORMAT_VERSIONS};
use crate::identity::{compute_body_fp, compute_turn_uid, normalize_body};
use crate::redact::{redact_text, RedactionEntry};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectCode {
    FormatVersionUnsupported,
    JsonMalformed,
    IdentityFieldMissing,
    CreatedAtMissing,
    BodyEmpty,
    TierClaimUnfalsifiable,
    DuplicateTurnUid,
    SeqNotStable,
    RoleUnknown,
    /// `project_hint` 是作用域键的一部分，只收逻辑名：路径/盘符/`.`/`..` 一律拒。
    ProjectHintNotLogical,
    /// G17：v3 输入读得成 JSON，但过不了 v3 契约闸（`validate_ingest_v3` 有错），
    /// 下行投影拒绝。与 `JsonMalformed`（形状就读不出）分开：这一条说的是
    /// "v3 语义不过关"，不是"JSON 坏了"。slug = `v3_invalid`。
    V3Invalid,
}

impl RejectCode {
    /// 拒收分型的运维面写法。stdout 报告和 `import_log.batch_reject_code` 共用这一个出口——
    /// 两处各自渲染过一次（`{:?}` 给 CamelCase、serde 给 snake_case），审计读数和当场报告就对不上了。
    pub fn slug(self) -> String {
        serde_json::to_value(self)
            .expect("closed enum with derived Serialize")
            .as_str()
            .expect("RejectCode serializes to a string")
            .to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Locator {
    pub session_index: usize,
    pub turn_index: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reject {
    pub loc: Locator,
    pub code: RejectCode,
    /// 只许放身份值与计数，绝不放正文（守卫 12）。tests\expose.rs 逐条断言。
    pub detail: String,
}

impl Reject {
    pub fn sess(i: usize, code: RejectCode, detail: impl Into<String>) -> Self {
        Reject {
            loc: Locator {
                session_index: i,
                turn_index: None,
            },
            code,
            detail: detail.into(),
        }
    }
    pub fn turn(si: usize, ti: usize, code: RejectCode, detail: impl Into<String>) -> Self {
        Reject {
            loc: Locator {
                session_index: si,
                turn_index: Some(ti),
            },
            code,
            detail: detail.into(),
        }
    }
}

/// 三层对象的合法键。与 `format.rs` 的 `deny_unknown_fields` 尽量对齐：
/// 结构体用它拒收，这里只用它在拒收后把「越界键在哪一层」定位出来。**`TURN_KEYS` 还没登记
/// `speaker`**（`format.rs` 那一侧已有这一格）⇒ 只带 `speaker` 的轮到这里会被判成越界键。
const EXCHANGE_FILE_KEYS: &[&str] = &["format_version", "sessions"];
const SESSION_KEYS: &[&str] = &[
    "session_key",
    "device_id",
    "source_agent",
    "machine",
    "project_hint",
    "last_saved_at_ms",
    // 身份层 I1：可选的三格主人身份（设计 `2026-10-27-principal-identity.md` §5）。
    "owner_principal_id",
    "principal_name",
    "principal_source",
    "turns",
];
const TURN_KEYS: &[&str] = &[
    "turn_uid",
    "seq",
    "tier",
    "role",
    "body",
    "created_at_ms",
];

/// 从 serde 的报错串里取未知字段名：`unknown field \`x\`, expected one of ...`。
fn reported_unknown_field(msg: &str) -> Option<&str> {
    msg.strip_prefix("unknown field `")?.split('`').next()
}

/// 键名与 `turn_uid` 都是生产者可控文本，可能长成一条路径或一段正文
/// （实测 `"C:/Users/x/.ssh/id_rsa"` 会原样回显）。只放行 token 形状（ASCII 字母数字与
/// `_-:.`，≤64，不以数字开头的键名另计），其余一律换成占位符：要定位的东西已经全在 `loc` 里，
/// 隐掉原串不损失任何排查能力。
///
/// 这条规则只有一个家：拒收 `detail` 与写入侧的计数器主语共用它（一个形状判据写两遍=下一次漂移）。
pub fn safe_token(name: &str) -> &str {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.')
        });
    if ok {
        name
    } else {
        "(redacted)"
    }
}

/// 这一层是否有越界键。`want=Some(w)` 只认 serde 报的那个键；`want=None` 认任意越界键。
fn has_offending_key(
    obj: &serde_json::Map<String, Value>,
    allowed: &[&str],
    want: Option<&str>,
) -> bool {
    obj.keys().any(|k| {
        if allowed.contains(&k.as_str()) {
            return false;
        }
        match want {
            Some(w) => k.as_str() == w,
            None => true,
        }
    })
}

/// 越界键所在的层级定位（顶层→会话 0；session→session_index；turn→session_index+turn_index）。
fn unknown_field_loc(doc: &Value, want: Option<&str>) -> Option<Locator> {
    let top = doc.as_object()?;
    if has_offending_key(top, EXCHANGE_FILE_KEYS, want) {
        return Some(Locator {
            session_index: 0,
            turn_index: None,
        });
    }
    let sessions = top.get("sessions")?.as_array()?;
    for (si, s) in sessions.iter().enumerate() {
        let Some(obj) = s.as_object() else { continue };
        if has_offending_key(obj, SESSION_KEYS, want) {
            return Some(Locator {
                session_index: si,
                turn_index: None,
            });
        }
        let Some(turns) = obj.get("turns").and_then(Value::as_array) else { continue };
        for (ti, t) in turns.iter().enumerate() {
            let Some(tobj) = t.as_object() else { continue };
            if has_offending_key(tobj, TURN_KEYS, want) {
                return Some(Locator {
                    session_index: si,
                    turn_index: Some(ti),
                });
            }
        }
    }
    None
}

/// `JsonMalformed` 的构造。未知字段是**输入侧拒收**（计划 Task 2 Step 5），detail 只回显**键名**
/// ——键不是正文；键的**值**可能是正文，绝不进输出（守卫 12）。
/// G17：`ingest_v3_project` 对 v3 输入的 serde 失败复用同一个构造器——拒收形状（含
/// 未知字段的层级定位）只有一个家，不在这里抄第二份。
pub(crate) fn json_reject(e: &serde_json::Error, bytes: &[u8]) -> Reject {
    let msg = e.to_string();
    if let Some(name) = reported_unknown_field(&msg) {
        let name = name.to_string();
        // serde 的报错串不带层级，回读一次文档把越界键定位到 session/turn。
        let doc = serde_json::from_slice::<Value>(bytes).ok();
        let loc = doc
            .as_ref()
            .and_then(|d| unknown_field_loc(d, Some(name.as_str())))
            .or_else(|| doc.as_ref().and_then(|d| unknown_field_loc(d, None)))
            .unwrap_or(Locator {
                session_index: 0,
                turn_index: None,
            });
        return Reject {
            loc,
            code: RejectCode::JsonMalformed,
            detail: format!("unknown field: {}", safe_token(&name)),
        };
    }
    // 其它形状错误：serde 的原串可能把值抄出来（实测 `invalid type: string "…", expected u32`
    // 会带上该字段的值），故只输出「类别 + 位置」——位置是偏移量，不是正文（守卫 12）。
    Reject::sess(
        0,
        RejectCode::JsonMalformed,
        format!(
            "malformed json: {:?} at line {} column {}",
            e.classify(),
            e.line(),
            e.column()
        ),
    )
}

pub fn parse(bytes: &[u8]) -> Result<ExchangeFile, Vec<Reject>> {
    let file: ExchangeFile =
        serde_json::from_slice(bytes).map_err(|e| vec![json_reject(&e, bytes)])?;
    if !SUPPORTED_FORMAT_VERSIONS.contains(&file.format_version) {
        return Err(vec![Reject::sess(
            0,
            RejectCode::FormatVersionUnsupported,
            format!(
                "declared={}, supported={:?}",
                file.format_version, SUPPORTED_FORMAT_VERSIONS
            ),
        )]);
    }
    let mut errs = Vec::new();
    for (si, s) in file.sessions.iter().enumerate() {
        if s.session_key.is_empty() || s.device_id.is_empty() || s.source_agent.is_empty() {
            errs.push(Reject::sess(
                si,
                RejectCode::IdentityFieldMissing,
                "session_key/device_id/source_agent",
            ));
            continue;
        }
        if let Some(hint) = s.project_hint.as_deref() {
            if !logical_scope_name(hint) {
                errs.push(Reject::sess(si, RejectCode::ProjectHintNotLogical, "project_hint"));
                continue;
            }
        }
        for (ti, t) in s.turns.iter().enumerate() {
            if t.created_at_ms.is_none() {
                errs.push(Reject::turn(si, ti, RejectCode::CreatedAtMissing, "created_at_ms"));
            }
            if t.body.is_empty() {
                errs.push(Reject::turn(si, ti, RejectCode::BodyEmpty, "body"));
            }
        }
    }
    if errs.is_empty() {
        Ok(file)
    } else {
        Err(errs)
    }
}

#[derive(Debug, Clone)]
pub struct NormalizedTurn {
    pub tier: TurnTier,
    pub turn_uid: String,
    pub body: String,
    pub body_fp: String,
    pub role: String,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone)]
pub struct SessionId {
    pub session_key: String,
    pub device_id: String,
    pub source_agent: String,
    pub machine: Option<String>,
    pub project_hint: Option<String>,
    pub last_saved_at_ms: Option<i64>,
    /// 身份层 I1（设计 `2026-10-27-principal-identity.md` §3.1/§5）：Agent 透传的主人身份三格。
    /// `None` = 这次导入没带身份 ⇒ **不盖章**（不造 `unknown` 行；`source=unknown` 是
    /// Agent 明确回答"不知道"时的主动值，不是缺省）。
    /// 写路径把它交给 `rc_ledger::transcript::insert_batch*` 在同一事务里追加一条 claim；
    /// `sessions.owner_principal_id` 那一列**不写**——它是 append-only 的旧锚，claim 表才是真相。
    pub owner_principal_id: Option<String>,
    /// 可读名，不是主键。
    pub principal_name: Option<String>,
    /// 章的来源（闭集见 [`PRINCIPAL_SOURCES`]）；`None` ⇒ 写侧取 `agent_runtime`。
    pub principal_source: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AcceptedTurn {
    pub session: SessionId,
    pub turn: NormalizedTurn,
    /// 轮级说话人（D 子 agent 接线）：`None` = 沿用 C1 的"按证据轮 role 推导"。
    ///
    /// 它不放进 [`NormalizedTurn`]：会话库的 `turns` 表没有这一列，两侧的按轮契约也不该因为
    /// 一条导入侧标注而分叉。写入路径在记忆侧（`record_turn_speakers`）消费它。
    pub speaker: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct BatchReport {
    pub accepted: Vec<AcceptedTurn>,
    pub rejects: Vec<Reject>,
    /// Some(_) ⇒ accepted 必为空。这就是"整批拒收"，它不是一次警告。
    pub batch_reject: Option<RejectCode>,
    /// 入库正文里被替换掉的凭据片段数（零正文）。只读计数，映射明文**不**进报告。
    pub redacted_spans: usize,
}

impl BatchReport {
    /// 被拒 turn 的 locator（`(session_index, turn_index)`），已去重、已排序。
    /// 一条 turn 可以同时踩中两个分型（缺 `created_at_ms` 且正文为空），而计数读数是
    /// "有几条 turn 被拒"——所以去重规则住这里，由 hub 的报告、用量计数器与 `import_log`
    /// 三个出口共用；两处各写一遍就是下一次漂移的开始。
    pub fn rejected_turns(&self) -> Vec<(usize, Option<usize>)> {
        let mut keys: Vec<(usize, Option<usize>)> = self
            .rejects
            .iter()
            .map(|r| (r.loc.session_index, r.loc.turn_index))
            .collect();
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    pub fn rejected_turn_count(&self) -> usize {
        self.rejected_turns().len()
    }
}

const VALID_ROLES: [&str; 3] = ["human", "agent", "system"];

/// `project_hint` 会进 `scope_key`；它必须是逻辑名，不是路径。
/// 允许空格与中文（项目名可以是自然语言），但不允许路径分隔符、盘符/ADS 的 `:`、
/// 控制字符，也不允许首尾空白或 `.`/`..`。换机器、换目录后同一项目必须仍是同一个作用域。
fn logical_scope_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.trim() == s
        && s != "."
        && s != ".."
        && !s.contains(['/', '\\', ':', '\0'])
        && !s.chars().any(char::is_control)
}

/// 声明档位可证伪 ⇒ 采纳；不可证伪 ⇒ 返回拒收码。未声明 ⇒ ContentOnly（禁猜高档）。
pub fn assign_tier(t: &RawTurn) -> (TurnTier, Option<RejectCode>) {
    let declared = t.tier.unwrap_or(TurnTier::ContentOnly);
    let falsifiable = match declared {
        TurnTier::IdBearing => t
            .turn_uid
            .as_deref()
            .map(|u| !u.trim().is_empty())
            .unwrap_or(false),
        TurnTier::StableSeq => t.seq.is_some(),
        TurnTier::ContentOnly => true,
    };
    if !falsifiable {
        return (declared, Some(RejectCode::TierClaimUnfalsifiable));
    }
    (declared, None)
}

pub fn normalize(file: ExchangeFile, _now_ms: i64) -> BatchReport {
    normalize_with(file, _now_ms, None)
}

/// 与 [`normalize`] 同一条路，外加一个**可选的**本地映射收集器。
///
/// `redaction_map` 只有调用方显式给了路径时才应当是 `Some`（`hub import --redaction-map`）：
/// 默认路径下只累加 [`BatchReport::redacted_spans`]，不留下任何明文副本。
///
/// 次序是硬约束：`turn_uid`/`body_fp` 用**脱敏前**的归一正文算，脱敏后的正文才进
/// `NormalizedTurn.body`。反过来写会让同一条轮在每次改名后换身份（重复导入不再幂等）。
pub fn normalize_with(
    file: ExchangeFile,
    _now_ms: i64,
    mut redaction_map: Option<&mut Vec<RedactionEntry>>,
) -> BatchReport {
    let mut rep = BatchReport::default();
    let mut uid_seen: std::collections::HashSet<String> = Default::default();

    'sessions: for (si, s) in file.sessions.iter().enumerate() {
        let mut seq_claim: std::collections::HashMap<i64, &str> = Default::default();
        for (ti, t) in s.turns.iter().enumerate() {
            let role_ok = VALID_ROLES.contains(&t.role.as_str());
            if !role_ok {
                // role 的值是生产者可控文本，可能是正文的一段（守卫 12）⇒ 只报字段名。
                rep.rejects.push(Reject::turn(si, ti, RejectCode::RoleUnknown, "role"));
            }
            let (tier, claim_err) = assign_tier(t);
            if let Some(code) = claim_err {
                rep.rejects.push(Reject::turn(si, ti, code, format!("tier={tier:?}")));
                rep.batch_reject = Some(code);
                break 'sessions;
            }
            if tier == TurnTier::IdBearing {
                let uid = t.turn_uid.clone().unwrap_or_default();
                if !uid_seen.insert(uid.clone()) {
                    rep.rejects.push(Reject::turn(si, ti, RejectCode::DuplicateTurnUid, safe_token(&uid)));
                    rep.batch_reject = Some(RejectCode::DuplicateTurnUid);
                    break 'sessions;
                }
            }
            // T1 的证伪。它承诺的是"这条序号能标识一个 turn"，所以判据是标识性而不是单调性：
            // 同一会话里同一序号被**不同 role** 声明 ⇒ 序号标识的是"某类 turn 的第 N 条"，不是
            // turn。没有这一条，给每条都填 0 的弱源就能永久伪装成 T1（spec §4：弱源伪装成强源
            // 的代价要等第一次重排导出才暴露，那时数据已经入库）。
            // 同序号 + 同 role + 不同正文是合法的**重生成/分支**（架构 §5 的 revision_group），不许拒。
            // 已因 role 非法被逐条拒的 turn 不参与登记——那是另一个平面的事，不该升级成整批拒收。
            if role_ok && tier == TurnTier::StableSeq {
                if let Some(ord) = t.seq {
                    match seq_claim.get(&ord) {
                        Some(prev) if *prev != t.role.as_str() => {
                            rep.rejects
                                .push(Reject::turn(si, ti, RejectCode::SeqNotStable, format!("ordinal={ord}")));
                            rep.batch_reject = Some(RejectCode::SeqNotStable);
                            break 'sessions;
                        }
                        Some(_) => {}
                        None => {
                            seq_claim.insert(ord, t.role.as_str());
                        }
                    }
                }
            }
        }
    }

    if rep.batch_reject.is_some() {
        rep.accepted.clear();
        return rep;
    }

    for (si, s) in file.sessions.iter().enumerate() {
        let session = SessionId {
            session_key: s.session_key.clone(),
            device_id: s.device_id.clone(),
            source_agent: s.source_agent.clone(),
            machine: s.machine.clone(),
            project_hint: s.project_hint.clone(),
            last_saved_at_ms: s.last_saved_at_ms,
            // 身份层 I1：**空串 = 未提供**。claim 的 `principal_id` 是 NOT NULL，空 id 不是身份，
            // 所以这里折成 `None`（= 这次导入不盖章），而不是写一条空 id 的章。
            // 三格都不做 trim/合并/推断：身份只由 Agent 侧给，工具只透传（设计 §2.1）。
            owner_principal_id: s.owner_principal_id.clone().filter(|v| !v.trim().is_empty()),
            principal_name: s.principal_name.clone().filter(|v| !v.trim().is_empty()),
            principal_source: s.principal_source.clone().filter(|v| !v.trim().is_empty()),
        };
        for (ti, t) in s.turns.iter().enumerate() {
            if rep
                .rejects
                .iter()
                .any(|r| r.loc == Locator { session_index: si, turn_index: Some(ti) })
            {
                continue;
            }
            let body = normalize_body(&t.body);
            // 归一后为空（原始串纯空白时 `parse` 的 BodyEmpty 看不住）⇒ 逐条拒收，不落空正文行。
            // 空 turn 的 uid/body_fp 都是对空 preimage 的哈希，落库后消费者读不到任何东西；
            // 逐条拒收会进导入报告，沉默不会。detail 只放字段名（守卫 12）。
            if body.is_empty() {
                rep.rejects
                    .push(Reject::turn(si, ti, RejectCode::BodyEmpty, "body"));
                continue;
            }
            let created_at_ms = t.created_at_ms.unwrap_or(0);
            // 身份必须用**原文**算：`uid`/`body_fp` 在脱敏之前落定（重复导入的幂等性就靠这一行）。
            let (tier, uid) = match t.tier.unwrap_or(TurnTier::ContentOnly) {
                TurnTier::IdBearing => (TurnTier::IdBearing, t.turn_uid.clone().unwrap()),
                TurnTier::StableSeq => (
                    TurnTier::StableSeq,
                    compute_turn_uid(&s.device_id, &s.session_key, &t.role, &body, created_at_ms),
                ),
                TurnTier::ContentOnly => (
                    TurnTier::ContentOnly,
                    compute_turn_uid(&s.device_id, &s.session_key, &t.role, &body, created_at_ms),
                ),
            };
            let body_fp = compute_body_fp(&body);
            // 入库正文的值级脱敏（设计 2026-10-25-extraction-system-blueprint.md 第 31–32 行）。
            // 只动 `body`；其它字段、轮数、身份一律不变。
            let redacted = redact_text(&body);
            if !redacted.spans.is_empty() {
                rep.redacted_spans += redacted.spans.len();
                if let Some(sink) = redaction_map.as_deref_mut() {
                    for span in &redacted.spans {
                        sink.push(RedactionEntry::new(
                            uid.clone(),
                            t.role.clone(),
                            *span,
                            &body[span.start..span.end],
                        ));
                    }
                }
            }
            rep.accepted.push(AcceptedTurn {
                session: session.clone(),
                turn: NormalizedTurn {
                    turn_uid: uid,
                    body_fp,
                    tier,
                    body: redacted.text,
                    role: t.role.clone(),
                    created_at_ms,
                },
                // 空白 = 没给：空串不是说话人，写进锚只会长出一格没有名字的 agent。
                speaker: t.speaker.clone().filter(|s| !s.trim().is_empty()),
            });
        }
    }
    rep
}

/// A39（阶段 3 协商；阶段 4 E1 恢复被 full plan 改掉的词表）：实体种类**六个**，均为字面常量。
/// 恢复依据 = 阶段 3 大纲 §1.0-14 的裁定文本；`event`/`other` 是 full plan T14 的自行偏离，不保留。
/// 阶段 4 起任何"加新种类"的申请必须先以设计裁定形式登记，再扩这一个常量。
pub const ENTITY_KINDS: [&str; 6] = [
    "person",   // 人
    "project",  // 项目（产品/工具/系统/课题组等）
    "org",      // 组织（公司/机构/团队）
    "tool",     // 工具（软件/设备/服务）
    "place",    // 地点（会议室/家/国家）
    "concept",  // 概念（算法/技术/方法论）
];

pub fn is_valid_entity_kind(s: &str) -> bool {
    ENTITY_KINDS.contains(&s)
}

/// canonical `anchor.kind` 的九个值（v4 抽取用）。与 v3 的 `ENTITY_KINDS` **不是同一张表**：
/// v3 的六值是消费行实体分类；九个里 person/org/place/concept 与 v3 同名，另有
/// agent/object/animal/work/event，v3 的 project/tool 不在其中。
pub const ANCHOR_KINDS: [&str; 9] = [
    "person", "agent", "org", "place", "object", "animal", "work", "event", "concept",
];

pub fn is_valid_anchor_kind(s: &str) -> bool {
    ANCHOR_KINDS.contains(&s)
}

/// `anchor.reality_status` 的六个值。DDL 的 CHECK 与 v4 契约共用这一份：
/// 写侧、parser 与测试都从它取，避免"契约文档一版、DDL 一版、parser 再一版"。
pub const REALITY_STATUSES: [&str; 6] =
    ["real", "fictional", "virtual", "abstract", "hypothetical", "unknown"];

/// `mention.discourse_role` 的四个值（DDL CHECK 的 Rust 侧对手）。
pub const DISCOURSE_ROLES: [&str; 4] = ["speaker", "addressee", "mentioned", "quoted"];

/// `mention.resolution` 的三个值（DDL CHECK 的 Rust 侧对手）。
pub const MENTION_RESOLUTIONS: [&str; 3] = ["resolved", "provisional", "unresolved"];

/// `session_principal_claim.source` 的四个值（设计 `2026-10-27-principal-identity.md` §3.1、
/// 工单规则 2）。DDL **不设 CHECK**（见 `rc-store/src/ddl.rs` 那段注释：新来源是常量扩展），
/// 所以闭集的唯一执法点是写侧 `rc_ledger::transcript::claim_principal`——它读的正是这一份
/// 字面量，避免"契约一版、DDL 一版、parser 再一版"。
pub const PRINCIPAL_SOURCES: [&str; 4] =
    ["agent_runtime", "human_claim", "unknown", "correction"];

pub fn is_valid_principal_source(s: &str) -> bool {
    PRINCIPAL_SOURCES.contains(&s)
}

/// A41（阶段 3 协商）：别名必须是源 turn 文本的子串——这是"agent 一句话指到它"的入口，
/// 别名不在源文里出现，那条记忆日后被人脑召回时**那条句柄也不在那段对话里**。
///
/// 本函数只做**判**，不做**接**——接线已落地：唯一调用者是
/// `rc_distill::extract::to_write_items`（在 `EntityContent` 落库前对 `aliases` 逐个判，
/// 不过的计 `aliases_dropped` 后丢弃）；此处只保证判据本身的形状与字面断言（与 T13"只上读侧"同族）。
/// 空别名 ⇒ `false`（一条不指任何东西的别名不是别名；守卫 10 的 pronoun 闸门另处已有）。
pub fn is_alias_in_source(alias: &str, source: &str) -> bool {
    !alias.is_empty() && source.contains(alias)
}
