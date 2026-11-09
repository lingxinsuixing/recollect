//! 一批 `BatchReport` → 转录层 + `usage_counter` + `import_log` 的唯一落库路径。
//! CLI 的 `import` 与 HTTP 的 `/v1/ingest` 共用它：两边各写一遍分组循环，
//! 下一次"只在本机导入那条路上加了计数器"就没有东西挡得住。

use rc_contract::{BatchReport, NormalizedTurn, SessionId};
use rc_ledger::counters;
use rc_ledger::import_log::{log_ingest, ImportLogRow};
use rc_ledger::transcript::{claim_principal, resolve_principal, NewPrincipalClaim};
use rc_memory::owner::OwnerCtx;
use rc_memory::writers_v4::{ensure_principal_anchor, principal_is_known, record_principal_mentions};
use rc_memory::{BatchCtx, Scope};
use rc_store::{Root, StoreError};
// ───────────── P0-1 主人身份通道：导入时的环境声明 ─────────────
//
// 缺的从来不是下游：`record_principal_mentions_after_import` 已经会读 claim、建主人锚、
// 写「我 / 我们」的 mention，写侧（`pass_fact`/`pass_event`/`pass_naming`）已经从 claim
// 取 `owner_principal_id`。缺的是 **claim 本身没有生产者**——`claim_principal` 在此之前
// 只有一个调用者，是运维手敲的 `hub principal claim`。这三格环境变量就是那个生产者：
// 它只在**导入**这条路上盖章，且只追加、绝不覆盖既有章。

/// 可读名（`示例主人` 这类**人名**，不是主机名/账号名）。设了它才启用自动盖章。
pub const PRINCIPAL_ENV_NAME: &str = "RECOLLECT_PRINCIPAL_NAME";
/// 稳定 id（可选）。缺席 ⇒ 按既有内容寻址算：`compute_mem_id(Anchor, "person", 名, "", scope)`。
pub const PRINCIPAL_ENV_ID: &str = "RECOLLECT_PRINCIPAL_ID";
/// 派生 id 时用的归属键（可选）。缺席 ⇒ 用**会话自己的** scope（与锚的 scope 同一口径）；
/// 给了 ⇒ 所有会话派生出同一个 id（同一个人跨项目仍是一个 principal，代价是 id 与 scope 解耦）。
pub const PRINCIPAL_ENV_SCOPE: &str = "RECOLLECT_PRINCIPAL_SCOPE";
/// 这一章的 `claimed_by`：把"导入自动盖"与运维手盖的 `hub` 在台账里分开。
pub const PRINCIPAL_ENV_CLAIMED_BY: &str = "env:RECOLLECT_PRINCIPAL_NAME";
/// 主人锚的 kind（`ensure_principal_anchor` 写的同一格）。`rc-memory` 里那个常量是私有的，
/// 所以这里复述一次字面量并留一行指向它：改 kind 时两处必须同时改，否则派生 id 与主人锚
/// 会落在两个 kind 上（一个查得到、一个查不到）。
const PRINCIPAL_PERSON_KIND: &str = "person";

/// 一次导入的环境声明。**只描述"谁"，不描述"哪一条会话"**：id 的派生要会话的 scope，
/// 所以派生发生在每个会话那一格（[`PrincipalDecl::principal_id_for`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrincipalDecl {
    name: String,
    id: Option<String>,
    scope_key: Option<String>,
}

impl PrincipalDecl {
    /// 从任意"取环境变量"的实现读三格。空白值一律当作**没给**（空白名字不是名字，
    /// 与 `claim_on` 对 `principal_name` 的处理同一条取舍）。
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let name = lookup(PRINCIPAL_ENV_NAME)?;
        let name = name.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let pick = |key: &str| {
            lookup(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        };
        Some(Self { name, id: pick(PRINCIPAL_ENV_ID), scope_key: pick(PRINCIPAL_ENV_SCOPE) })
    }

    /// 进程环境那一份。**这是导入路径上唯一读这三格的地方**（CLI 与 HTTP 共用 `commit`）。
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// 该会话的稳定 `principal_id`。显式给了就用显式的；否则按内容寻址派生——
    /// 复用 `compute_mem_id` 的 person 口径，与主人锚同一把尺子（不新造算法）。
    pub fn principal_id_for(&self, session_scope: &Scope) -> String {
        match &self.id {
            Some(id) => id.clone(),
            None => {
                let scope = match &self.scope_key {
                    Some(key) => Scope { kind: session_scope.kind, key: key.clone() },
                    None => session_scope.clone(),
                };
                rc_memory::compute_mem_id(
                    rc_memory::MemTable::Anchor,
                    PRINCIPAL_PERSON_KIND,
                    &self.name,
                    "",
                    &scope,
                )
            }
        }
    }
}


// ───────────── W14c 环境/账号属性：导入时的环境声明 ─────────────
//
// 用户 2026-11-06 的两句话定了这一片：
// "以上所有信息都归属于示例主人这个实体，不过指定名字是归属于哪个实体环境/设备在使用。"
// "agent 查询『某个账号昵称』，那么能获取的信息是示例主人在 B 站的账户的昵称。"
//
// ⇒ 账号名**不是人**，是环境锚（`kind='object'`）上的 `attribute`。与 [`PrincipalDecl`] 同一处
// （`commit`，CLI 与 HTTP 共用）、同一把尺子：环境变量缺席 ⇒ 一格都不写。

/// 主机名。**给了它才启用整条环境声明**：属性要挂在某个环境锚上，没有主机名就没有锚。
pub const ENV_HOSTNAME: &str = "RECOLLECT_ENV_HOSTNAME";
/// 品牌（`Acer`）。
pub const ENV_BRAND: &str = "RECOLLECT_ENV_BRAND";
/// 操作系统账号（`u`）。
pub const ENV_OS_USER: &str = "RECOLLECT_ENV_OS_USER";
/// B 站账号昵称（`甲某随行`）。
pub const ENV_ACCOUNT_BILIBILI: &str = "RECOLLECT_ACCOUNT_BILIBILI";
/// Qoder 用户名（`example-account`）。
pub const ENV_ACCOUNT_QODER: &str = "RECOLLECT_ACCOUNT_QODER";

/// 一次导入的环境声明：**一个主机名 + 它身上挂的属性**。
///
/// `device_id` 不在这里：它不是"环境自称的名字"，而是这条链自己用的设备键，从批次取
/// （[`EnvironmentDecl::attributes`] 的调用方补）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvironmentDecl {
    hostname: String,
    attributes: Vec<(&'static str, String)>,
}

impl EnvironmentDecl {
    /// 从任意"取环境变量"的实现读五格。空白值一律当作没给（与 [`PrincipalDecl::from_lookup`]
    /// 同一条取舍）。**主机名缺席 ⇒ 整条不启用**（`None`）：其余四格没有落点，写出去就是
    /// 四条挂在空气上的属性。
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Option<Self> {
        let pick = |key: &str| {
            lookup(key).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
        };
        let hostname = pick(ENV_HOSTNAME)?;
        // 主机名本身也是属性（键表的第一格）：它进 `attribute`，锚的 canonical 也取它。
        let mut attributes: Vec<(&'static str, String)> =
            vec![(rc_memory::ATTR_HOSTNAME, hostname.clone())];
        for (env_key, attr_key) in [
            (ENV_BRAND, rc_memory::ATTR_BRAND),
            (ENV_OS_USER, rc_memory::ATTR_OS_USER),
            (ENV_ACCOUNT_BILIBILI, rc_memory::ATTR_BILIBILI_ACCOUNT),
            (ENV_ACCOUNT_QODER, rc_memory::ATTR_QODER_ACCOUNT),
        ] {
            if let Some(value) = pick(env_key) {
                attributes.push((attr_key, value));
            }
        }
        Some(Self { hostname, attributes })
    }

    /// 进程环境那一份。**这是导入路径上唯一读这五格的地方**（CLI 与 HTTP 共用 `commit`）。
    pub fn from_env() -> Option<Self> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// `(属性键, 值)`，顺序固定（键表顺序）⇒ 同一组环境变量两次导入写出的行序一致。
    pub fn attributes(&self) -> &[(&'static str, String)] {
        &self.attributes
    }
}


#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Committed {
    pub inserted: usize,
    pub skipped_duplicate: usize,
    /// 被 purge 墓碑挡住的 turn 数：不落行、不分配 commit_seq，但要在读数里看得见。
    pub tombstoned: usize,
    /// 本次导入产生的最大提交号；全重复/整批拒收为 None。
    pub commit_seq: Option<u64>,
    pub epoch: Option<u64>,
}

pub struct CommitInput<'a> {
    pub started_at_ms: i64,
    pub now_ms: i64,
    pub device_id: Option<&'a str>,
    pub exchange_sha256: &'a str,
    pub devices: &'a [String],
    pub rep: &'a BatchReport,
    /// 整行被拒、没进 `rep` 的 turn 数（HTTP 才有；CLI 是 0）。
    /// 只影响审计行的 `turns_rejected`，不影响写入。
    pub line_rejected_turns: i64,
}

/// 计数器主语得是一个"能归因的设备"。纯空白不是（它能过 `parse`，因为不是空串），
/// 形状像路径或正文的也不是——主语会被 `doctor` 原样 echo 回运维面，所以这一层必须在**写入侧**
/// 挡住：doctor 不依赖 rc-contract，它读到什么就印什么。判据用 `safe_token`（同一个形状规则，
/// 不是第二个正则）。
pub fn device_subject(claimed: &str) -> Option<&str> {
    if claimed.trim().is_empty() || rc_contract::safe_token(claimed) != claimed {
        return None;
    }
    Some(claimed)
}

fn resolve_device(devices: &[String], session_index: usize) -> &str {
    devices.get(session_index).map(String::as_str).unwrap_or("unknown")
}

/// 单会话文件才谈得上"这次导入来自哪台设备"。多会话、JSON 本体就读不出（`devices` 空）、
/// 空白或形状不可归因时报 None：审计行里一个猜错的主语比空主语更难查。
pub fn sole_device(devices: &[String]) -> Option<&str> {
    (devices.len() == 1)
        .then(|| devices[0].as_str())
        .and_then(device_subject)
}

fn bump_rejects_by_device(
    root: &mut Root,
    devices: &[String],
    rep: &BatchReport,
    now_ms: i64,
) -> Result<(), StoreError> {
    let mut counts: Vec<(&str, i64)> = Vec::new();
    // 去重口径住在 BatchReport 里（报告 / 计数器 / import_log 三处共用）。
    for (si, _) in rep.rejected_turns() {
        let id = device_subject(resolve_device(devices, si)).unwrap_or("unknown");
        match counts.iter_mut().find(|(i, _)| *i == id) {
            Some(e) => e.1 += 1,
            None => counts.push((id, 1)),
        }
    }
    // counts 只在真有拒收时非空 ⇒ 零增量的行根本不会被创建，doctor 读不到幽灵计数。
    for (id, n) in counts {
        counters::bump(root.ledger_mut(), "device", id, "ingest_turns_rejected", n, now_ms)?;
    }
    Ok(())
}

/// 整行拒收里已知的 turn 数。`ingest_turns_rejected` 是"被拒 turn 的总数"，
/// 所以这些 turn 也得进同一个计数器；`signature_turns` 是它的子集，只做可观测。
pub fn bump_line_rejects(
    root: &mut Root,
    device_id: &str,
    turns: i64,
    signature_turns: i64,
    now_ms: i64,
) -> Result<(), StoreError> {
    let id = device_subject(device_id).unwrap_or("unknown");
    if turns > 0 {
        counters::bump(root.ledger_mut(), "device", id, "ingest_turns_rejected", turns, now_ms)?;
    }
    if signature_turns > 0 {
        counters::bump(
            root.ledger_mut(), "device", id,
            "ingest_turns_rejected_signature", signature_turns, now_ms,
        )?;
    }
    Ok(())
}

pub fn commit(root: &mut Root, input: CommitInput<'_>) -> Result<Committed, StoreError> {
    let mut out = Committed {
        epoch: Some(rc_ledger::transcript::center_epoch(root.transcript())?),
        ..Default::default()
    };
    // P0-1：这次导入有没有声明主人身份。**缺席 ⇒ 与今天逐字相同**（一格都不写）。
    let declared = PrincipalDecl::from_env();
    // W14c：这次导入有没有声明"这台环境叫什么、它用着哪些账号"。**缺席 ⇒ 一格都不写**。
    let environment = EnvironmentDecl::from_env();
    if input.rep.batch_reject.is_none() {
        // 按会话分组写，一次一个事务；崩溃窗口由 turn_uid 主键吸收（Task 8）。
        let mut groups: Vec<(SessionId, Vec<NormalizedTurn>)> = Vec::new();
        for acc in input.rep.accepted.iter() {
            match groups.iter_mut().find(|(s, _)| {
                s.session_key == acc.session.session_key && s.device_id == acc.session.device_id
            }) {
                Some(g) => g.1.push(acc.turn.clone()),
                None => groups.push((acc.session.clone(), vec![acc.turn.clone()])),
            }
        }
        for (session, turns) in &groups {
            // purge 之后同 scope 的新导入默认拒绝：旧设备/旧备份不能把已删会话交回来。
            if rc_ledger::transcript::tombstone_blocks(
                root.transcript(),
                &session.session_key,
                &session.device_id,
                None,
            )? {
                out.tombstoned += turns.len();
                counters::bump(
                    root.ledger_mut(),
                    "device",
                    device_subject(&session.device_id).unwrap_or("unknown"),
                    "ingest_turns_tombstoned",
                    turns.len() as i64,
                    input.now_ms,
                )?;
                continue;
            }
            let ins = rc_ledger::transcript::insert_batch_committed(
                root.transcript_mut(), session, turns, input.now_ms, "import",
            )?;
            out.inserted += ins.inserted;
            out.skipped_duplicate += ins.skipped_duplicate;
            if let Some(seq) = ins.commit_seq {
                out.commit_seq = Some(out.commit_seq.map_or(seq, |current| current.max(seq)));
            }
            if ins.epoch.is_some() {
                out.epoch = ins.epoch;
            }
            // P0-1：环境声明的主人身份在这里落成一条 claim。**必须排在上面那句之后**——
            // 会话行（scope 的来源）这时才存在。已有有效章 ⇒ 一条都不动（`human_claim`
            // 与 `correction` 比环境变量更有分量，自动盖章不许把它们顶掉）。
            if let Some(decl) = declared.as_ref() {
                // 幂等命中的会话（已有有效章）一格不写；导入路径不消费这个读数，
                // 前置补章路径（pre_claim_declared_principals）才按它记账。
                let _wrote = claim_declared_principal(root.transcript(), session, decl, input.now_ms)?;
            }
            // W14c：环境声明落成"一个 object 锚 + 它身上的属性"。同样排在上面那句之后
            // （章与 scope 都要读落库之后的会话行），也同样**只在有声明时**动库。
            if let Some(decl) = environment.as_ref() {
                write_declared_environment(root, session, decl, input.now_ms)?;
            }
            // 一组必有一条以上，所以这里不存在"零增量"的分支；§9.5 的读数按上行 turn 数算，
            // 幂等命中的重传同样计入（失控节点反复刷同一批也要在计数里看得见）。
            counters::bump(
                root.ledger_mut(),
                "device",
                device_subject(&session.device_id).unwrap_or("unknown"),
                "ingest_turns_accepted",
                turns.len() as i64,
                input.now_ms,
            )?;
        }
    }
    bump_rejects_by_device(root, input.devices, input.rep, input.now_ms)?;
    // 与 insert_batch 刻意分开的事务：崩溃在两者之间，会话层留下真行、日志缺席——Task 8 的语义。
    let batch_code = input.rep.batch_reject.map(|c| c.slug()).unwrap_or_default();
    let row = ImportLogRow {
        started_at_ms: input.started_at_ms,
        finished_at_ms: input.now_ms,
        device_id: input.device_id,
        exchange_sha256: input.exchange_sha256,
        sessions_in: input.rep.accepted.iter().map(|a| &a.session.session_key)
            .collect::<std::collections::HashSet<_>>().len() as i64,
        turns_accepted: input.rep.accepted.len() as i64,
        turns_rejected: input.rep.rejected_turn_count() as i64 + input.line_rejected_turns,
        batch_reject_code: &batch_code,
    };
    log_ingest(root.ledger(), &row)?;
    Ok(out)
}

/// 把这一个会话的**环境声明**落成一条 claim（P0-1 的生产者；W37-A2/G13③ 起提升为可复用
/// 入口：导入路径之外，`hub principal claim-env`（daily-drain 蒸馏前的前置补章）调同一个实现，
/// 不许再长第二份盖章 SQL）。
///
/// 三条口径（都在这一处执法，别处不许再写第二份）：
/// 1. **幂等 / 不覆盖**：现行章已知（[`resolve_principal`] 读到 `claim.is_some()` 且 id 不是
///    空白/`unknown`）⇒ 直接返回、一格都不写（回 `false`）。章是追加式的，运维手盖的
///    `human_claim` / `correction` 比环境变量更有分量——自动盖章把它顶掉，等于"每次导入
///    都改写主人是谁"；重跑同一库也由此保证不重复写；
/// 2. **现行章是 `unknown` 时可以补**：`unknown` 是 Agent 明确答的"不知道"，不是一个人的章。
///    环境变量是更强的声明，所以这里追加一条并让它 `supersedes` 那条 `unknown`
///    （历史仍在，`hub principal list` 读得到整条链）；
/// 3. **scope 从落库之后的会话行回读**（口径与 `record_principal_mentions_after_import` /
///    `pass_naming` 的 `COALESCE(scope_key, project_hint, session_key)` 逐字相同）：
///    `OwnerCtx::synthetic` 那种猜法会让派生 id 与 distill 的 scope 漂移。
///
/// 返回值：`true` = 这次调用真追加了一条章；`false` = 幂等命中（已有有效章），一格没写。
pub fn claim_declared_principal(
    t: &rc_store::TranscriptHandle,
    session: &SessionId,
    decl: &PrincipalDecl,
    now_ms: i64,
) -> Result<bool, StoreError> {
    if let Some(existing) = resolve_principal(t, &session.session_key, &session.device_id)? {
        if existing.claim.is_some() && principal_is_known(&existing.principal_id) {
            return Ok(false);
        }
    }
    let row = t.conn().query_row(
        "SELECT COALESCE(scope_kind,'project'), COALESCE(scope_key, project_hint, session_key)
           FROM sessions WHERE session_key=?1 AND device_id=?2",
        rusqlite::params![session.session_key, session.device_id],
        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
    );
    let (scope_kind, scope_key) = match row {
        Ok(v) => v,
        // 会话行不在（理论上刚写完）：不猜归属，这一组跳过。
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let scope = Scope { kind: rc_memory::owner::normalize_scope_kind(&scope_kind), key: scope_key };
    let principal_id = decl.principal_id_for(&scope);
    claim_principal(
        t,
        NewPrincipalClaim {
            session_key: &session.session_key,
            device_id: &session.device_id,
            principal_id: &principal_id,
            principal_name: Some(decl.name()),
            // 闭集第一值：这一章来自 Agent 的运行时声明，不是人当场盖的。
            source: "agent_runtime",
            claimed_by: PRINCIPAL_ENV_CLAIMED_BY,
            reason: None,
        },
        now_ms,
    )?;
    Ok(true)
}

/// [`pre_claim_declared_principals`] 的读数（CLI 的文本行与 `--json` 是同一组字段）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PreClaimReport {
    /// 扫到的"无章"会话数（现行章缺失或 `unknown`）。
    pub scanned: usize,
    /// 本次真正追加的章数（含顶掉 `unknown` 的那条）。
    pub claimed: usize,
    /// 幂等命中（扫描到调用之间章已被补上）的会话数；常态为 0，一格兜底计数。
    pub already_claimed: usize,
}

/// 对**无章会话**按环境声明补章（W37-A2/G13③②：daily-drain 蒸馏前的前置那一枪）。
///
/// "无章"的判定一次 SQL 交回：现行章（`claimed_at_ms` 最新一条）缺失，或它的 id 是
/// `unknown`（口径 2 允许补）；没有章时回落看会话列——列上已有已知 id 的会话不需要章
/// 也能解析（`resolve_principal` 的回落臂），**不在补章面里**：用环境声明的 id 顶掉
/// 导出器/运维写在列上的身份，是替人改归属。墓碑会话不单独排除：章对已 purge 的会话
/// 是惰性行（轮已删、无读者），多一条无害。
///
/// `decl` 缺席（操作员没声明 `RECOLLECT_PRINCIPAL_NAME`）⇒ 一格不写、全零读数——
/// "没声明"不是故障。幂等：逐会话走 [`claim_declared_principal`]（口径 1），
/// 重跑同一库 `claimed=0`、章链不长第二行。
pub fn pre_claim_declared_principals(
    root: &Root,
    decl: Option<&PrincipalDecl>,
    now_ms: i64,
) -> Result<PreClaimReport, StoreError> {
    let mut report = PreClaimReport::default();
    let Some(decl) = decl else { return Ok(report); };
    let mut st = root.transcript().conn().prepare(
        "SELECT s.session_key, s.device_id
           FROM sessions s
          WHERE COALESCE(
                  (SELECT c.principal_id FROM session_principal_claim c
                    WHERE c.session_key = s.session_key AND c.device_id = s.device_id
                    ORDER BY c.claimed_at_ms DESC LIMIT 1),
                  s.owner_principal_id, '') IN ('', 'unknown')
          ORDER BY s.session_key, s.device_id",
    )?;
    let candidates = st
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(st);
    report.scanned = candidates.len();
    for (session_key, device_id) in candidates {
        // 只带键：盖章读的是库里的会话行，声明的其余格一个都不从这结构体取。
        let session = SessionId {
            session_key,
            device_id,
            source_agent: String::new(),
            machine: None,
            project_hint: None,
            last_saved_at_ms: None,
            owner_principal_id: None,
            principal_name: None,
            principal_source: None,
        };
        if claim_declared_principal(root.transcript(), &session, decl, now_ms)? {
            report.claimed += 1;
        } else {
            report.already_claimed += 1;
        }
    }
    Ok(report)
}

/// W14c：把一个会话的**环境声明**落成"一个环境锚 + 它身上的属性"。
///
/// 三条口径：
/// 1. **归属必须先有主**：锚的 `resource_owner.owner_principal_id` 取 [`resolve_principal`]
///    读到的那一格，且必须 `claim.is_some()`（与 `record_principal_mentions_after_import`
///    的守卫逐字相同）。没有章/身份未知 ⇒ **一条都不写**——造一个没有主人的设备锚，正是
///    W14b 要消掉的那种形状；
/// 2. **scope/归属从落库之后的会话行回读**（口径同 `pass_naming`/
///    `record_principal_mentions_after_import` 的那条 SQL，默认值也一样）：`OwnerCtx::synthetic`
///    那种猜法会让环境锚与 distill 的 scope 漂移；
/// 3. **`device_id` 那条属性的值取批次的设备标识**（`session.device_id`），不是环境变量：
///    设备键是链自己用的，环境变量只描述"这台机器叫什么、挂了哪些账号"。
///
/// 幂等：锚与属性都由内容寻址 id + `ON CONFLICT DO NOTHING` 兜住（`writers_attr`），
/// 同一批重导不长第二行。
fn write_declared_environment(
    root: &Root,
    session: &SessionId,
    decl: &EnvironmentDecl,
    now_ms: i64,
) -> Result<(), StoreError> {
    let Some(principal) = resolve_principal(
        root.transcript(),
        &session.session_key,
        &session.device_id,
    )?
    else {
        return Ok(());
    };
    if principal.claim.is_none() || !principal_is_known(&principal.principal_id) {
        return Ok(());
    }
    let row = root.transcript().conn().query_row(
        "SELECT COALESCE(owner_agent_id, source_agent), COALESCE(visibility,'private_agent'),
                COALESCE(retention,'default'), COALESCE(scope_kind,'project'),
                COALESCE(scope_key, project_hint, session_key)
           FROM sessions WHERE session_key=?1 AND device_id=?2",
        rusqlite::params![session.session_key, session.device_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        },
    );
    let (owner_agent, visibility, retention, scope_kind, scope_key) = match row {
        Ok(v) => v,
        // 会话行不在（理论上刚写完）：不猜归属，这一组跳过。
        Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let owner = OwnerCtx {
        owner_principal_id: Some(principal.principal_id.clone()),
        owner_agent_id: owner_agent,
        visibility: rc_memory::owner::parse_visibility(&visibility),
        retention,
    };
    let scope = Scope {
        kind: rc_memory::owner::normalize_scope_kind(&scope_kind),
        key: scope_key,
    };
    let ctx = BatchCtx {
        source_agent: session.source_agent.clone(),
        device_id: session.device_id.clone(),
    };
    let anchor_id = rc_memory::ensure_environment_anchor(
        root.memory(),
        decl.hostname(),
        &ctx,
        &owner,
        &scope,
        now_ms,
    )
    .map_err(mem_write_error)?;
    // 设备标识是"这台环境自己"的属性：值取批次的设备键，追加在声明属性之后。
    let mut items: Vec<(&'static str, String)> = decl.attributes().to_vec();
    items.push((rc_memory::ATTR_DEVICE_ID, ctx.device_id.clone()));
    rc_memory::put_attribute_rows(root.memory(), &anchor_id, &items, &scope, &ctx, now_ms)
        .map_err(mem_write_error)?;
    Ok(())
}

/// `rc-memory` 的写错误 → `StoreError`。载荷只带字段名/句柄（`MemError` 自己已经过守卫 12
/// 的消毒），不把路径或正文带进导入报告。
fn mem_write_error(e: rc_memory::MemError) -> StoreError {
    StoreError::Sql(format!("environment write: {e}"))
}

/// D31：human 轮正文里的「我 / 我们」→ 主人锚 mention（`commit` 之后的独立一遍）。
///
/// **CLI 与 HTTP 共用**：`hub import` 与 `/v1/ingest` 都在 `commit` 成功返回之后调它。
/// 必须在 transcript commit 之后调用——claim 与 `sessions` 归属行这时才是真值。
/// **墓碑守卫不可删**：`rep.accepted` 仍含被墓碑挡住的轮（`commit` 只 `continue`），
/// 不在这里再挡一次，purge 之后重导就会给已删会话重新写 mention。
///
/// 与 CLI 的 `record_turn_speakers_after_import` 并列，但四条口径不同：
/// 1. **只处理 `role == "human"` 的轮**：agent/system 轮的「我」不是会话主人；
/// 2. **先查墓碑**：`commit` 对墓碑会话是 `continue`，但 `rep.accepted` 里仍有这些轮——
///    不在这里再挡一次，purge 之后重导就会给已删会话重新写 mention。既有 speaker 钩子
///    没有这道守卫，这里不复制那个洞；
/// 3. **身份必须盖过章**：`resolve_principal` 的 `claim.is_some()`（与闸的
///    `KnownPrincipal` 同一口径）。只有 `sessions.owner_principal_id` 回落列不算盖章，
///    否则会替"没盖章的会话"放宽闸 6c 第 3 支；
/// 4. **归属/scope 从落库之后的 `sessions` 行回读**（口径同 `pass_naming` 的那条 SQL），
///    不用 `OwnerCtx::synthetic` 猜——合成 scope 会与 distill 漂移。
///
/// 失败只回错误参数给调用方（CLI 是 `operational("store write failed")`，HTTP 侧忽略、
/// 不改 200），不打印细节。
pub fn record_principal_mentions_after_import(
    root: &Root,
    rep: &BatchReport,
    now_ms: i64,
) -> Result<(), Box<dyn std::error::Error>> {
    // 分组与 speaker 钩子同形：一次导入的会话数是个位数，线性查找即可。
    let mut groups: Vec<(&rc_contract::SessionId, Vec<(String, String)>)> = Vec::new();
    for acc in &rep.accepted {
        if acc.turn.role != "human" {
            continue;
        }
        match groups.iter_mut().find(|(s, _)| {
            s.session_key == acc.session.session_key && s.device_id == acc.session.device_id
        }) {
            Some((_, turns)) => turns.push((acc.turn.turn_uid.clone(), acc.turn.body.clone())),
            None => groups.push((
                &acc.session,
                vec![(acc.turn.turn_uid.clone(), acc.turn.body.clone())],
            )),
        }
    }
    for (session, turns) in groups {
        if turns.is_empty() {
            continue;
        }
        // ① 墓碑：已删会话不再长 mention。
        if rc_ledger::transcript::tombstone_blocks(
            root.transcript(),
            &session.session_key,
            &session.device_id,
            None,
        )? {
            continue;
        }
        // ② 身份：只有 claim 算盖章；`unknown`/空 再挡一次。
        let Some(principal) = rc_ledger::transcript::resolve_principal(
            root.transcript(),
            &session.session_key,
            &session.device_id,
        )?
        else {
            continue;
        };
        if principal.claim.is_none() || !principal_is_known(&principal.principal_id) {
            continue;
        }
        // ③ 归属/scope：落库之后的真值（默认值口径同 `pass_naming`，不发明新枚举）。
        let row = root.transcript().conn().query_row(
            "SELECT COALESCE(owner_agent_id, source_agent), COALESCE(visibility,'private_agent'),
                    COALESCE(retention,'default'), COALESCE(scope_kind,'project'),
                    COALESCE(scope_key, project_hint, session_key)
               FROM sessions WHERE session_key=?1 AND device_id=?2",
            rusqlite::params![session.session_key, session.device_id],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            },
        );
        let (owner_agent, visibility, retention, scope_kind, scope_key) = match row {
            Ok(v) => v,
            // 会话行不在（理论上 commit 已写过）：不猜归属，整组跳过。
            Err(rusqlite::Error::QueryReturnedNoRows) => continue,
            Err(e) => return Err(Box::new(e)),
        };
        let owner = OwnerCtx {
            owner_principal_id: Some(principal.principal_id.clone()),
            owner_agent_id: owner_agent,
            visibility: rc_memory::owner::parse_visibility(&visibility),
            retention,
        };
        let scope = Scope {
            kind: rc_memory::owner::normalize_scope_kind(&scope_kind),
            key: scope_key,
        };
        // ④ 主人锚：幂等（同 canonical 同 scope 一格）；身份未知已在上一步挡住。
        let Some(anchor) = ensure_principal_anchor(
            root.memory(),
            &principal.principal_id,
            principal.principal_name.as_deref(),
            &owner,
            &scope,
            now_ms,
        )? else {
            continue;
        };
        let ctx = BatchCtx {
            source_agent: session.source_agent.clone(),
            device_id: session.device_id.clone(),
        };
        record_principal_mentions(root.memory(), &ctx, &turns, &anchor.id, now_ms)?;
    }
    Ok(())
}
