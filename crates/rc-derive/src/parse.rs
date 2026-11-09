//! 四键契约（裁定 12 + 批次 M 段 B）：relationships / chains / timeline 缺任一整批丢弃；
//! **mentions 是第四键**：缺键或非数组 ⇒ 降级为空 + `mentions_missing` 计数（不整批丢）；
//! 枚举外单条拒收 + 计数；co_occurs 在两处各判、同一计数；
//! chains 另有三道形态拒收（机制空白 / 环节数 < 2 / 链内重复句柄，批次③ 工单 §2），
//! 外加 **chain type 三值枚举**（causes/enables/prevents，批次 M），同一计数；
//! 外加**证据闸的两道**（派发档 I-2 · Phase 2.4）：机制句是套话 ⇒ 码 `no_mechanism`、
//! 一条依据行都没点名 ⇒ 码 `no_evidence`，两道都**只拦机制类 relation**（P-4），
//! 计数口径与既有各道逐字相同（一条链只计一次 `rejected`，且都在 push 之前）；
//! mentions 的 **verbatim 单条拒收**：名字必须逐字出现在该行的 title 或 body 里（代词/空名同理）；
//! 零行是一个**信号**而不是丢批理由：非空批四数组全空 ⇒ `ZeroOutput`；`run_derive` 把它
//! 读成"模型明确答了没有可推导内容"，照推游标（缺键/格式错才是契约丢批）。
//!
//! 行 id 用位置别名 r1..rn 喂给 LLM（"行 id 与指纹不进 prompt"——T4 实施时严格走这条）；
//! parse 层把 r<n> 映射回真实行 id（输入的 `&[RowFacts]` = 顺序就是 alias 编号）。
//! 每一处别名都走同一支解析——from / to / chain 的 from[] / **三个键各自的 basis**，
//! 所以 `DerivePlan` 出去的全部是真实行 id，位置别名不越过 parse 这一层（T4 把 `basis_ids` 写进归属时，
//! 落库的是行 id 而不是 "r1" 这种批内坐标）。mentions 的行别名走 `alias_to_row`（要读那行的
//! title/body 做 verbatim），名字→实体 id 的解析**不在本层**（那是 `put_mentions` 的事）。
//! 越界（r99）或空批的解析失败一律**单条拒收**，不炸批；只有契约级失败（缺键、JSON 不合法）才是 Contract 错。
//! **唯一的例外是 timeline 的 anchor**：它解析不动只把 `anchor_id` 置 `None`、该条照收且不计数——
//! 裁定 13 要的就是"无锚 ⇒ NULL ms + `unknown`"，整条拒收反而抹掉了写侧要落 `unknown` 的那一支形状。

use rc_memory::RowFacts;
use rc_memory::writers_x::{MECHANISM_RELATIONS, is_placeholder_mechanism};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relationship {
    pub from_id: String,
    pub to_id: String,
    pub kind: String,
    pub reason: String,
    pub basis_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chain {
    pub from_ids: Vec<String>,
    pub mechanism: String,
    /// 链头类型：∈ causes/enables/prevents（本层已验值域，写侧不再判）。
    pub causal_type: String,
    pub basis_ids: Vec<String>,
}

/// 一条 mention（行提到实体）。名字→实体 id 的解析归 `put_mentions`，本层只落行 id + 名字。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention { pub row_id: String, pub name: String }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Timeline {
    pub anchor_id: Option<String>,
    pub offset_ms: Option<i64>,
    /// 已归一的绝对日期（`YYYY-MM-DD`）。`None` = 走相对锚那一支。
    pub date: Option<String>,
    /// 写下这个日期的那条依据行（绝对日期支的锚）。
    pub date_anchor_id: Option<String>,
    pub body_ref: String,
    pub basis_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivePlan {
    pub relationships: Vec<Relationship>,
    pub chains: Vec<Chain>,
    pub timeline: Vec<Timeline>,
    pub mentions: Vec<Mention>,
    /// `mentions` 缺键或非数组 ⇒ 1；正常数组（含空数组）⇒ 0。**不整批丢**。
    pub mentions_missing: usize,
    pub rejected: usize,
    /// 证据闸那几条拒收的**封闭类别串**（[`REJECT_NO_MECHANISM`] 与 [`REJECT_NO_EVIDENCE`]），
    /// 与 `rejected` 同一批条目一一对应（每拒一条 push 一支，`rejected` 也 +1）。
    /// 为什么它不在 `DeriveReadout`/cycle 的 JSON 面上：那两处在 `rc-distill`/`rc-cycle`，
    /// 本任务的落点之外，而 P-5 定的是"读数只加不改"——先在本层把"这一条为什么被拒"钉成
    /// 可分诊的一支串，读数面今天仍是一格 `rejected`（合并计数）。**这一格今天的读者是
    /// 用例与分诊（`crates/rc-derive/tests/evidence_gate.rs`），不是运维面板**，
    /// 已在报告里登记为未解决点。
    pub rejected_codes: Vec<&'static str>,
    pub rows: usize,
}

#[derive(Debug, Error)]
pub enum DeriveError {
    #[error("contract violation: {0}")]
    Contract(String),
    /// 四数组全空。**不是**丢批理由：调用方 `run_derive` 把它读成合格的空答复并推游标；
    /// 缺键、非数组与 JSON 畸形才走 `Contract`。
    #[error("batch produced zero derive rows")]
    ZeroOutput,
    /// 库侧失败（读归属、写边/时间线）。**必须与 `Contract` 分家**：T5 的 `run_derive` 按族分诊——
    /// 契约族 = 整批丢 + `note_attempt`，库故障族 = 报错、不计 attempts。把 `put_edges` 的
    /// `MemError` 折进 `Contract`，运维面就把"写不进去"念成"模型答得不合格"（读数说谎）。
    /// 这一支在 T3 没有生产者（`parse_derive_output` 不碰库），由 T4 的落库段填。
    #[error("store: {0}")]
    Store(rc_memory::MemError),
    /// LLM 侧失败（超时 / 配额 / 服务不可用 / 回形不合）。同样**不许折进 `Contract`**：
    /// 契约族是"这批被输出契约拒收"（计 attempts、可能触发停轮），LLM 族是"这一轮没跑成"
    /// （既不推水位也不计 attempts——hub 的 `cycle_failure` 同律）。
    /// T3 无生产者，由 T4 的 `backend.complete` 填。载荷用 `#[from]`：与 `DistillError::Llm` 同形。
    #[error(transparent)]
    Llm(#[from] rc_llm::LlmFailure),
}

const RELATIONSHIP_KINDS: &[&str] = &["part_of", "depends_on", "owns", "located_in", "role"];
const CHAIN_TYPES: &[&str] = &["causes", "enables", "prevents"];

// ── 证据闸的封闭类别串（派发档 I-2 · Phase 2.4 · §2.2）──────────────────────────────
// 守卫 12：只许 `&'static str`，带正文的原串一律不进这一格。
// **不与既有码撞名**（本仓有"新闸抢码序把别的判据变成 dropped"的先例）。既有码序逐条 grep 过：
// 推导家族今天的类别串是 `DeriveReadout::drop_reason` 那一格（唯一值 `"schema"`，与
// `rc_memory::issues::DERIVE_SCHEMA_CODE` 同一个词）+ `DeriveExit::Fault` 的六支运维措辞
// （`FAULT_STORE`/`FAULT_READ` 与四支 LLM），**而单条拒收今天一个码都没有**——`rejected`
// 是一格纯计数。所以这两支是接在该序之后新加的一族：不复用、不覆盖 `"schema"`，也不动
// `drop_reason`——那一格说的是"整批丢"，这两支说的是"这一条不落"，两件事两格账。
/// 机制类关系交的是套话（或空白）而不是机制句 ⇒ 该条不落。判据的家在
/// `rc_memory::writers_x::PLACEHOLDER_MECHANISMS`（整串相等，不是"含"）。
pub const REJECT_NO_MECHANISM: &str = "no_mechanism";
/// 机制类关系一条依据行都没点名 ⇒ 该条不落（"没有证据就不落"的那一半）。
pub const REJECT_NO_EVIDENCE: &str = "no_evidence";

fn alias_to_id<'a>(alias: &str, ids: &'a [String]) -> Option<&'a str> {
    // 形态：r1 / r2 / r3 ……数字 1-based，越界或空 ⇒ None
    let n: usize = alias.strip_prefix('r')?.parse().ok()?;
    if n == 0 || n > ids.len() { return None; }
    Some(ids[n - 1].as_str())
}

/// mention 的行别名 → 那一行的 `RowFacts`：verbatim 检查要读它的 title/body。
/// 形态与 `alias_to_id` 同一支（r1 1-based、越界/空 ⇒ None）。
fn alias_to_row<'a>(alias: &str, rows: &'a [RowFacts]) -> Option<&'a RowFacts> {
    let n: usize = alias.strip_prefix('r')?.parse().ok()?;
    if n == 0 { return None; }
    rows.get(n - 1)
}

/// basis 与 from/to 同一支解析：任一别名解析不动 ⇒ 整条拒收（`None` 由调用方计数）。
/// 依据行是"这条推导能被谁重算出来"的唯一凭据——留着未解析的 "r98" 就等于把一条说不出依据的
/// 推导放行进库，那正是反例四支里"推测当事实"的形状。
/// 守备范围只到"给了的别名必须解得动"：`basis` 键缺席或 `[]` 经 `unwrap_or_default` 成空向量 ⇒
/// `Some(vec![])`，**本支不拒收**——当时的判断是"一条没有任何依据行的推导落库时怎么署名"归写侧裁。
/// 那一句今天**只对结构归属那一支成立**：机制类关系（`causes`/`enables`/`prevents`/`responds_to`）
/// 的"依据行为零"已由本层的 ④ 号闸门在模型面拒掉并给出码 `no_evidence`（派发档 I-2 · Phase 2.4，
/// P-4 划的界：结构归属不许被要求机制句与依据行，否则得到的是一批假拒）。
fn resolve_basis(aliases: &[String], ids: &[String]) -> Option<Vec<String>> {
    aliases.iter().map(|a| alias_to_id(a, ids).map(String::from)).collect()
}

pub fn parse_derive_output(json_text: &str, batch: &[RowFacts]) -> Result<DerivePlan, DeriveError> {
    let ids: Vec<String> = batch.iter().map(|r| r.key.id.clone()).collect();
    let v: Value = serde_json::from_str(json_text)
        .map_err(|e| DeriveError::Contract(format!("json: {e}")))?;
    let obj = v.as_object()
        .ok_or_else(|| DeriveError::Contract("root is not an object".into()))?;
    for key in ["relationships", "chains", "timeline"] {
        if !obj.contains_key(key) {
            return Err(DeriveError::Contract(format!("missing key `{key}`")));
        }
    }

    let mut plan = DerivePlan { relationships: vec![], chains: vec![], timeline: vec![],
                                mentions: vec![], mentions_missing: 0,
                                rejected: 0, rejected_codes: vec![], rows: 0 };

    // relationships
    let rels = obj.get("relationships").and_then(Value::as_array)
        .ok_or_else(|| DeriveError::Contract("relationships not array".into()))?;
    for r in rels {
        let from = r.get("from").and_then(Value::as_str).unwrap_or("");
        let to = r.get("to").and_then(Value::as_str).unwrap_or("");
        let kind = r.get("kind").and_then(Value::as_str).unwrap_or("");
        let reason = r.get("reason").and_then(Value::as_str).unwrap_or("");
        let basis: Vec<String> = r.get("basis").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        // 拒收口径：kind 枚举外 / from 或 to 越界 / basis 含越界别名（同一条只计一次）
        // 后半支 `|| kind == "co_occurs"` 是**死支**：它本来就不在五项枚举里，前一支已经拦下，
        // 撤掉它没有任何用例会红 ⇒ 无可指向它的变异。留着防的是"日后有人把 co_occurs 加进枚举"，
        // 顺带让"共现不是关系"这条裁定在代码里点名，而不只活在枚举表的一次缺席里——
        // 链那一侧的同名词是有牙的（mechanism 无枚举，撤之则该用例的链半支红）。
        if !RELATIONSHIP_KINDS.contains(&kind) || kind == "co_occurs" {
            plan.rejected += 1; continue;
        }
        let (Some(from_id), Some(to_id)) = (alias_to_id(from, &ids), alias_to_id(to, &ids)) else {
            plan.rejected += 1; continue;
        };
        let Some(basis_ids) = resolve_basis(&basis, &ids) else {
            plan.rejected += 1; continue;
        };
        plan.relationships.push(Relationship {
            from_id: from_id.into(), to_id: to_id.into(),
            kind: kind.into(), reason: reason.into(), basis_ids,
        });
    }

    // chains
    let chains = obj.get("chains").and_then(Value::as_array)
        .ok_or_else(|| DeriveError::Contract("chains not array".into()))?;
    for c in chains {
        let mechanism = c.get("mechanism").and_then(Value::as_str).unwrap_or("");
        let causal_type = c.get("type").and_then(Value::as_str).unwrap_or("");
        let from_aliases: Vec<String> = c.get("from").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let basis: Vec<String> = c.get("basis").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        if mechanism == "co_occurs" {
            plan.rejected += 1; continue;  // 裁定 12：co_occurs 当因果拒收
        }
        // 批次③ 的三道"宁拒不错落"（工单 §2）。三道的计数口径与既有三道逐字相同：
        // 一条链只计一次 `rejected`（每道都是 `+= 1; continue`），且都在 push 之前。
        // ① 机制只有空白 ⇒ 拒收。`mechanism` 键缺失时上一段把它折成 `""`，而链头的正文
        //    就是这句话（标题 `"derive-"` 非空 ⇒ 写侧的"标题非空"那道闸门拦不住它），
        //    放到写侧就会落一行**空正文的因果事件**。判据只用 `trim`：rc-derive 不依赖
        //    rc-contract，`normalize_body` 递不进来（加依赖就撞本批"不动 Cargo.lock"的红线）；
        //    更强的那一道住 `rc_memory::writers_x::put_chains`，守在库门口，两层不冗余。
        if mechanism.trim().is_empty() {
            plan.rejected += 1; continue;
        }
        // ①b 机制句是**套话** ⇒ 拒收，码 [`REJECT_NO_MECHANISM`]（派发档 I-2 · Phase 2.4）。
        // 既有那一道只拦"空白"，拦不住"时间相邻""同一个实体"这种**有字但没说出机制**的答复——
        // 而计划 D3 要防的正是那一型（A 在 B 之前只说明顺序，不证明因果；共现、同实体、时间相邻
        // 都不足以成边）。判据与闭集的家都在 `rc_memory::writers_x`（`PLACEHOLDER_MECHANISMS`：
        // **整串相等**，不是"含"——与 `PRONOUNS`/`check_alias` 同一把尺子，误拦比漏拦贵）。
        // 只对机制类 relation 判（P-4）：链头的 `type` 就是它的 relation（causes/enables/prevents
        // 三支全在闭集里），`co_occurs` 那一支已由上面拦下；`type` 缺失或域外时这一支**跳过**，
        // 让下面那道既有的枚举闸门按既有口径拒它（一条链只计一次拒收）。
        if MECHANISM_RELATIONS.contains(&causal_type) && is_placeholder_mechanism(mechanism) {
            plan.rejected += 1; plan.rejected_codes.push(REJECT_NO_MECHANISM); continue;
        }
        // ② 环节数 < 2 ⇒ 拒收。一个节点的"链"断不出任何方向，落库只是一行带 `ord = 0` 的孤行。
        if from_aliases.len() < 2 {
            plan.rejected += 1; continue;
        }
        // 全部别名都要解析得动；任一失败 ⇒ 单条拒收
        let mut ok = true;
        let mut ids_resolved = Vec::with_capacity(from_aliases.len());
        for a in &from_aliases {
            match alias_to_id(a, &ids) {
                Some(id) => ids_resolved.push(id.to_string()),
                None => { ok = false; break; }
            }
        }
        if !ok { plan.rejected += 1; continue; }
        // ③ 链内重复句柄 ⇒ 拒收。**为什么在 parse 拦而不是在写侧容**：环节行落 `provenance`
        //    的主键 `(mem_table, mem_id, turn_uid)`，同一个句柄在同一条链里必撞自己 ⇒
        //    `INSERT OR IGNORE` 静默吞掉一个环节、`ord` 序列出现缺口且不报错——写侧"容"的形状
        //    就是那句静默，所以这一格只能在模型面拒。查重按**解析后的真实行 id**：
        //    两个不同别名解到同一行同样是重复句柄。
        if ids_resolved.iter().enumerate().any(|(i, id)| ids_resolved[..i].contains(id)) {
            plan.rejected += 1; continue;
        }
        // chain type（批次 M 段 B）：三值封闭枚举，与关系 kind 同口径**单条拒收**。
        // 位置在三道既有形态闸门之后 ⇒ 一条坏链只计一次拒收（前三道已先 continue 掉的那些不再走到这里）。
        if !CHAIN_TYPES.contains(&causal_type) {
            plan.rejected += 1; continue;
        }
        let Some(basis_ids) = resolve_basis(&basis, &ids) else {
            plan.rejected += 1; continue;
        };
        // ④ 一条依据行都没点名 ⇒ 拒收，码 [`REJECT_NO_EVIDENCE`]（"没有证据就不落"的那一半）。
        // 这一格补的是既有注释里点名过、但一直没落地的那一支：`resolve_basis` 对 `basis` 缺键或
        // `[]` 只给 `Some(vec![])`（"不拒收"），当时写的是"归属怎么署名是写侧的问题"——
        // 而写侧的结论是**环节行占了出处、依据行为零**：库里看得见"这条链连着谁"，
        // 看不见"这条链凭什么"。机制类关系没有可重算的出处就不该落库（计划 D3/D6：无证据不落、
        // inferred 必须有 basis），所以判据在模型面这一层就得做，并且给得出码。
        // 同样只对机制类 relation 判（P-4）。
        if MECHANISM_RELATIONS.contains(&causal_type) && basis_ids.is_empty() {
            plan.rejected += 1; plan.rejected_codes.push(REJECT_NO_EVIDENCE); continue;
        }
        plan.chains.push(Chain { from_ids: ids_resolved, mechanism: mechanism.into(),
                                 causal_type: causal_type.into(), basis_ids });
    }

    // timeline：时间锚两口径在 T4 落写侧校验（绝对日期检出 / 相对表述 anchored/unknown），
    // parse 这一层做形态解析 + 别名解析；basis 越界与关系/链同口径单条拒收
    let tlines = obj.get("timeline").and_then(Value::as_array)
        .ok_or_else(|| DeriveError::Contract("timeline not array".into()))?;
    for t in tlines {
        let anchor = t.get("anchor").and_then(Value::as_str).and_then(|a| alias_to_id(a, &ids)).map(String::from);
        let offset_ms = t.get("offset_ms").and_then(Value::as_i64);
        let body_ref = t.get("body_ref").and_then(Value::as_str).unwrap_or("").into();
        let basis: Vec<String> = t.get("basis").and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let Some(basis_ids) = resolve_basis(&basis, &ids) else {
            plan.rejected += 1; continue;
        };
        // C29 绝对日期：三判据缺一即单条拒收。
        // ① 归一得动；② 与相对锚二选一；③ 至少一条依据行的 title/body 逐字含同一日期。
        let mut date = None;
        let mut date_anchor_id = None;
        if let Some(raw) = t.get("date").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
            let Some(canon) = crate::date::canonical_date(raw) else {
                plan.rejected += 1; continue;
            };
            if anchor.is_some() || offset_ms.is_some() {
                plan.rejected += 1; continue;
            }
            let mut found = None;
            for bid in &basis_ids {
                let Some(row) = batch.iter().find(|r| &r.key.id == bid) else { continue; };
                if crate::date::dates_in(&row.title).contains(&canon)
                    || crate::date::dates_in(&row.body).contains(&canon) {
                    found = Some(bid.clone()); break;
                }
            }
            let Some(aid) = found else {
                plan.rejected += 1; continue;
            };
            date = Some(canon); date_anchor_id = Some(aid);
        }
        // C28：归属要有落点（无锚且无依据 ⇒ 拒收）。
        if anchor.is_none() && date.is_none() && basis_ids.is_empty() {
            plan.rejected += 1; continue;
        }
        plan.timeline.push(Timeline { anchor_id: anchor, offset_ms, date, date_anchor_id,
                                      body_ref, basis_ids });
    }

    // mentions（四键之一，批次 M 段 B）：缺键/非数组 ⇒ 空 + `mentions_missing`（**不整批丢**）。
    // 单条拒收：空/纯空白名字（`"".contains("")` 恒真，verbatim 拦不住它）、行别名解析不动、
    // 名字没有逐字出现在该行的 title/body 里。一个名字都不解析成实体 id——那是 `put_mentions` 的事。
    let (mention_items, mentions_missing): (&[Value], usize) = match obj.get("mentions") {
        Some(Value::Array(a)) => (a.as_slice(), 0),
        Some(_) => (&[], 1),      // 非数组
        None => (&[], 1),         // 缺键
    };
    for m in mention_items {
        let row_alias = m.get("row").and_then(Value::as_str).unwrap_or("");
        let name = m.get("name").and_then(Value::as_str).unwrap_or("");
        // 空/纯空白名字先拒：`"".contains("")` 恒真，verbatim 检查拦不住它。
        if name.trim().is_empty() { plan.rejected += 1; continue; }
        let Some(row) = alias_to_row(row_alias, batch) else { plan.rejected += 1; continue; };
        if !(row.title.contains(name) || row.body.contains(name)) { plan.rejected += 1; continue; }
        plan.mentions.push(Mention { row_id: row.key.id.clone(), name: name.into() });
    }
    plan.mentions_missing = mentions_missing;

    // rows = 落库条数 + 拒收条数（裁定 12 计数口径；率不上浮、显示层除法）。
    // accepted mentions 不进 `rows`（那三格是"三键落库条数"）；mention 拒收走 `rejected` ⇒ 自动进 rows。
    plan.rows = plan.relationships.len() + plan.chains.len() + plan.timeline.len() + plan.rejected;
    // 零输出判据（控制器代拍 R-M-1）：mentions 是四键之一 ⇒ 三键空 + 一条合法 mention 是契约内答复，
    // 不是"模型什么都没答"；缺 mentions 键且三键皆空仍走 ZeroOutput（不放宽旧案）。
    if plan.rows == 0 && plan.mentions.is_empty() {
        return Err(DeriveError::ZeroOutput);
    }
    Ok(plan)
}
