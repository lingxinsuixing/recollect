//! 读闸的**唯一实现点**（S1b-C4a；设计 ownership-persistence §4.1）。
//!
//! 这一层回答一个问题：**这位读者能不能看见这一行**。它不回答"哪几行最相关"（那是
//! `rc_search::rank` 的事）、也不改候选集的排序——所以权限**不许**拼进 `reader::facts()`
//! 的 SQL：一次 `ORDER BY` 的候选集被权限动过，"排序不真"就同时成了权限的函数。
//! 形状因此是"一次装载、纯内存判定"：[`GateIndex::load`] 把窄表 `resource_owner` 与
//! 有效的 `resource_grant` 读进来，之后每一行只做哈希查找（§4.1 的代价账）。
//!
//! 三条不许漂的口径：
//! ① **fail-closed**：没有归属行、缺表、认不出的 visibility，只能往更窄的一档折。
//!    唯一的两个例外都在规则表的前三条里——Maintenance 面（中心/运维）与"行
//!    `source_agent` 恰等于 viewer agent"（同 agent 自有行，旧库全靠它才不变空）；
//! ② **解析只有一套**：`visibility`/`scope_kind` 走 [`owner::parse_visibility`] 与
//!    [`owner::normalize_scope_kind`]，不在这里长青第二把尺子；
//! ③ **不泄露存在性**：本模块的返回值只有 `Ok(())` 与 `Deny`，被拒的行在调用方那里
//!    与"查不到"同形（`recall` 的句柄臂把两者都推进 `unresolved_handles`）。
//!
//! `viewer == None` 在 [`facts_visible`]/[`lookup_visible`] 里等同 Maintenance
//! （**只给中心 CLI/测试/旧调用**；mcp 的生产面必须给 `Some`）。它连装载都省掉：
//! 结果与 Maintenance 逐位相同，而"没有读者"这件事不该向库要一次全表扫描。
use crate::keys::{MemKey, Scope, ScopeKind};
use crate::owner;
use crate::owner::Visibility;
use crate::reader::{self, RowFacts};
use crate::MemError;
use rc_store::MemoryHandle;
use std::collections::HashMap;

/// 读者所在的那一面。它只影响两件事：Maintenance 在规则表第 1 条直接放行；
/// grant 的 `capability` 按面取（Agent 要 `recall`、Human 要 `export`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Agent,
    Human,
    Maintenance,
}

/// 一次读的读者身份。字段全 `pub`：调用方按面自己拼，构造器只负责"面"与默认值，
/// 不给一个"忘了设 principal/scope 也能造出来"的私有形状。
///
/// 不 derive `PartialEq`：`Scope` 本身没有相等语义（`keys.rs` 故意不 derive），
/// 而读闸比较 scope 的地方在 [`scope_eq`] 里逐列比——那比"整个 `Viewer` 相等"更窄。
#[derive(Debug, Clone)]
pub struct Viewer {
    pub agent_id: String,
    pub principal_id: Option<String>,
    pub scope: Option<Scope>,
    pub surface: Surface,
}

impl Viewer {
    pub fn agent(agent_id: impl Into<String>) -> Self {
        Viewer { agent_id: agent_id.into(), principal_id: None, scope: None, surface: Surface::Agent }
    }

    pub fn human(agent_id: impl Into<String>) -> Self {
        Viewer { agent_id: agent_id.into(), principal_id: None, scope: None, surface: Surface::Human }
    }

    /// 中心/运维面：规则表第 1 条放行。`agent_id` 是常量而不是空串——
    /// 一个空串会在"行 source_agent 为空"的那些格上碰巧命中，而它不是任何一个 agent。
    pub fn maintenance() -> Self {
        Viewer {
            agent_id: "maintenance".to_string(),
            principal_id: None,
            scope: None,
            surface: Surface::Maintenance,
        }
    }

    pub fn with_principal(mut self, p: impl Into<String>) -> Self {
        self.principal_id = Some(p.into());
        self
    }

    pub fn with_scope(mut self, s: Scope) -> Self {
        self.scope = Some(s);
        self
    }
}

/// 拒绝的**类别**。调用方今天只把它读成"不许"（`recall` 两臂都不区分原因），
/// 保留分格是为了读数说明"为什么看不见"时不必再造一份同义词表。
///
/// `GrantMissing`/`GrantExpired` 今天**没有生产者**：过期、撤回、面不匹配、target 不匹配的
/// grant 在 [`GateIndex::load`] 就已经被滤掉（fail-closed ⇒ 回落到第 10 条按 visibility 分格）。
/// 把它们从词表里删掉的代价是下一轮（C4b 的读数面）得重新加回来，所以这里留着并写明现状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deny {
    NoOwnerRow,
    PrivateAgent,
    PersonNeedsGrant,
    ScopeMismatch,
    OrgUnsupported,
    GrantMissing,
    GrantExpired,
    TableMissing,
}

impl Deny {
    /// 八个原因变体的**闭集清单**（B4 分诊计数的键序来源；加变体当场长度编译错，
    /// 与 `GateCell::ALL` / `AgentErrorKind::ALL` 同一条"封闭枚举自报家门"的纪律）。
    pub const ALL: [Deny; 8] = [
        Deny::NoOwnerRow,
        Deny::PrivateAgent,
        Deny::PersonNeedsGrant,
        Deny::ScopeMismatch,
        Deny::OrgUnsupported,
        Deny::GrantMissing,
        Deny::GrantExpired,
        Deny::TableMissing,
    ];

    /// 稳定的机读 slug（分诊计数与日志面的键）。词表的家在这里，消费面不许另写一份
    /// （与 `RejectCode::slug()` / `AgentErrorKind::slug()` 同一个理由：渲染处只许一个）。
    pub fn slug(self) -> &'static str {
        match self {
            Deny::NoOwnerRow => "no_owner_row",
            Deny::PrivateAgent => "private_agent",
            Deny::PersonNeedsGrant => "person_needs_grant",
            Deny::ScopeMismatch => "scope_mismatch",
            Deny::OrgUnsupported => "org_unsupported",
            Deny::GrantMissing => "grant_missing",
            Deny::GrantExpired => "grant_expired",
            Deny::TableMissing => "table_missing",
        }
    }
}

/// 归属行的读闸视图：只留判定要用的格（`created_by_agent`/`retention`/时间戳与读无关）。
/// `anchor_kind` 是 W37-G12 扩的一列：本行若是锚行，这里带它的 `anchor.kind`
/// （装载时 JOIN 一并取，`decide` 不逐行回库——"一次装载、纯内存判定"不破）。
/// card 行 JOIN 不中 ⇒ `None`；非环境锚的锚行（person/agent/…）带它们各自的 kind。
#[derive(Debug, Clone)]
struct OwnerFacts {
    owner_principal_id: Option<String>,
    owner_agent_id: String,
    visibility: Visibility,
    scope: Scope,
    anchor_kind: Option<String>,
}

impl OwnerFacts {
    /// 环境锚判定（设计 W37 §3）：本行是 `kind='object'` 的锚行。全树写这个 kind 的
    /// 入口按裁定以 [`crate::writers_attr::ENV_ANCHOR_KIND`] 这一个常量为准。
    fn is_environment_anchor(&self) -> bool {
        self.anchor_kind.as_deref() == Some(crate::writers_attr::ENV_ANCHOR_KIND)
    }
}

/// 一条**已经过了** target/capability/data_class/未撤回/未过期五道门的授权。
/// 剩下的一道（scope 覆不覆盖这一行）要对着那行的 owner scope 判，所以留到 [`Grant::covers`]。
#[derive(Debug, Clone)]
struct Grant {
    /// `scope_kind='global'`：覆盖所有行（`scope_key` 是空串，不参与比较）。
    global: bool,
    scope_kind: ScopeKind,
    scope_key: String,
}

impl Grant {
    fn covers(&self, owner_scope: &Scope) -> bool {
        if self.global {
            return true;
        }
        self.scope_kind == owner_scope.kind && self.scope_key == owner_scope.key
    }
}

/// 一次装载、多次判定。键是 `MemKey::key_string()`（`"<table>:<id>"`）——它正是
/// `resource_owner` 的两列拼法，且 `MemKey::parse` 按第一个 `':'` 切分 ⇒ 可逆。
pub struct GateIndex {
    viewer: Viewer,
    owners: HashMap<String, OwnerFacts>,
    grants: Vec<Grant>,
    table_present: bool,
}

impl GateIndex {
    /// 一次读全量 `resource_owner`（窄表）+ 有效 grant；之后纯内存判定。
    ///
    /// `table_present=false` 不是错误：旧副本/旧快照没有这张表（`MEMORY_VERSION` 没有 bump，
    /// 表存在性不能靠版本判，§9-4），此时规则表第 2 条只放行自有行。grant 表缺失同理——
    /// 它至多让"放宽"少一条，天然是 fail-closed 的方向，所以只当空集。
    ///
    /// Maintenance 面连装载都省掉：规则表第 1 条先放行，一次损坏的侧表不该把运维面拖死。
    pub fn load(m: &MemoryHandle, v: &Viewer, now_ms: i64) -> Result<Self, MemError> {
        if v.surface == Surface::Maintenance {
            return Ok(GateIndex {
                viewer: v.clone(),
                owners: HashMap::new(),
                grants: Vec::new(),
                table_present: true,
            });
        }
        let conn = m.conn();
        let table_present = table_exists(conn, "resource_owner")?;
        let mut owners = HashMap::new();
        if table_present {
            // W37-G12：锚行多带一列 `anchor.kind`（环境锚判定要用）。LEFT JOIN：
            // card 行 JOIN 不中 ⇒ kind 为 NULL；`resource_owner` 里没有归属行的行根本不进本表。
            // attribute 行没有自己的归属行（writers_attr 档头②：归属由所在环境锚的那一行承载，
            // 读侧 `attribute_owners` 按锚取主人），所以"attribute 经锚取 kind"没有第二行可标——
            // 锚行被下面的环境锚例外拦下时，挂在它身上的属性读数同样过不来。
            let mut st = conn.prepare(
                "SELECT ro.mem_table, ro.mem_id, ro.owner_principal_id, ro.owner_agent_id,
                        ro.visibility, ro.scope_kind, ro.scope_key, a.kind
                   FROM resource_owner ro
                   LEFT JOIN anchor a ON ro.mem_table='anchor' AND a.id=ro.mem_id")?;
            let rows = st.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?,
                    r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?, r.get::<_, Option<String>>(7)?,
                ))
            })?;
            for row in rows {
                let (mem_table, mem_id, principal, agent, visibility, kind, key, anchor_kind) = row?;
                owners.insert(
                    format!("{mem_table}:{mem_id}"),
                    OwnerFacts {
                        owner_principal_id: principal,
                        owner_agent_id: agent,
                        visibility: owner::parse_visibility(&visibility),
                        scope: Scope { kind: owner::normalize_scope_kind(&kind), key },
                        anchor_kind,
                    },
                );
            }
        }
        let grants = if table_exists(conn, "resource_grant")? {
            load_grants(conn, v, now_ms)?
        } else {
            Vec::new()
        };
        Ok(GateIndex { viewer: v.clone(), owners, grants, table_present })
    }

    /// 规则表（§4.1，按序短路）。`Ok(())` = 这一行对这位读者可见。
    pub fn decide(&self, f: &RowFacts) -> Result<(), Deny> {
        let v = &self.viewer;
        // 1. 中心/运维面：唯一一个"不看归属"的读者。
        if v.surface == Surface::Maintenance {
            return Ok(());
        }
        // 2. 缺表（旧副本）：只放行"行署名恰等于 viewer"的那一族，其余降成 TableMissing。
        if !self.table_present {
            return if producer_is_viewer(f, v) { Ok(()) } else { Err(Deny::TableMissing) };
        }
        // 3. 没有归属行：同上，旧的抽取行（owner 还没回填）因此不会整库消失。
        let Some(o) = self.owners.get(&f.key.key_string()) else {
            return if producer_is_viewer(f, v) { Ok(()) } else { Err(Deny::NoOwnerRow) };
        };
        self.decide_owner_row(o)
    }

    /// 与 [`decide`](Self::decide) **同一套规则**，但没有 `RowFacts`：用于边端点这类
    /// "只知道 `table:id`"的场合（C4b2b）。
    ///
    /// 前三条按句柄能表达的只有两条半：Maintenance 放行、缺表拒；**没有归属行 ⇒ 拒**——
    /// 第 2/3 条那条"行署名恰等于 viewer"要读行的 `source_agent`，句柄上没有这一格，
    /// 所以"证明不了来源就不放行"（fail-closed）。这是它比 `decide` **唯一**更窄的一格，
    /// 方向只能是更窄：它不会凭空多放行一行。
    pub fn allows(&self, mem_table: &str, mem_id: &str) -> bool {
        self.allows_reason(mem_table, mem_id).is_ok()
    }

    /// [`allows`](Self::allows) 的**带原因形**（B4 分诊计数用）：调用方要"为什么被拒"时用它，
    /// 旧调用（`allows`）委托它 ⇒ 两条路**同一套规则**，不存在第二份真值表。
    /// 前三条按句柄能表达的只有两条半（与 `allows` 档头同一句）：Maintenance 放行、缺表拒
    /// （`TableMissing`）、无归属行拒（`NoOwnerRow`）——"行署名恰等于 viewer"那半条在这里
    /// 同样证明不了（句柄上没有 `source_agent`），fail-closed 方向不变。
    pub fn allows_reason(&self, mem_table: &str, mem_id: &str) -> Result<(), Deny> {
        let v = &self.viewer;
        if v.surface == Surface::Maintenance {
            return Ok(());
        }
        if !self.table_present {
            return Err(Deny::TableMissing);
        }
        match self.owners.get(&format!("{mem_table}:{mem_id}")) {
            Some(o) => self.decide_owner_row(o),
            None => Err(Deny::NoOwnerRow),
        }
    }

    /// 与 [`decide`](Self::decide) 同一套规则的行过滤；[`facts_visible`] 与 `rc-agent` 的
    /// `export`（事实行 + 边的一次装载）共用**这一个**实现，不各写一份 `retain`。
    pub fn filter(&self, facts: Vec<RowFacts>) -> Vec<RowFacts> {
        facts.into_iter().filter(|f| self.decide(f).is_ok()).collect()
    }

    /// 第 4–10 条（归属行上的五格 + grant）：`decide` 与 [`allows`](Self::allows) 的公共实现。
    /// 两条路在这一段上**没有任何分叉**——分叉只允许出现在"有没有行事实可看"的前三条。
    fn decide_owner_row(&self, o: &OwnerFacts) -> Result<(), Deny> {
        let v = &self.viewer;
        // 4. owner 永远能读自己（**同 principal 不自动放行**：这一条只比 agent）。
        if o.owner_agent_id == v.agent_id {
            return Ok(());
        }
        // 4b. 同 principal 的异 agent：同主人的 agent 默认互读（W37-G12，设计 §1/§2）。
        // fail-closed 的两半都在 `principal_eq` 里：viewer 或 owner 任一边没有非空
        // principal ⇒ 这一条永不命中 ⇒ 拒读不变。**环境锚例外**（设计 §3）：
        // `kind='object'` 的锚不落这条，继续走 6–10 的 visibility 分格——
        // private_agent 的环境锚对同 principal 的异 agent 仍是 `PrivateAgent`；
        // owner 自读在第 4 条已经放行，例外不改 owner 自读。
        if !o.is_environment_anchor()
            && principal_eq(v.principal_id.as_ref(), o.owner_principal_id.as_ref())
        {
            return Ok(());
        }
        // 5. 有效 grant（target/capability/data_class/时效已在装载时过掉，这里只剩 scope）。
        if self.grants.iter().any(|g| g.covers(&o.scope)) {
            return Ok(());
        }
        // 6–10. 隐式受众：visibility 是上界，逐档判。
        match o.visibility {
            Visibility::Public => Ok(()),
            Visibility::Project => {
                if scope_eq(v.scope.as_ref(), &o.scope) {
                    Ok(())
                } else {
                    Err(Deny::ScopeMismatch)
                }
            }
            Visibility::Person => {
                if v.surface == Surface::Human
                    && principal_eq(v.principal_id.as_ref(), o.owner_principal_id.as_ref())
                {
                    Ok(())
                } else {
                    Err(Deny::PersonNeedsGrant)
                }
            }
            // 9. 组织身份今天没有任何来源：认得出这一档，但只能拒。
            Visibility::Org => Err(Deny::OrgUnsupported),
            Visibility::PrivateAgent => Err(Deny::PrivateAgent),
        }
    }
}

/// 行署名恰等于 viewer：**唯一的**"没有归属证据也放行"的那一格。
/// 空串永远不算（锚行读侧补的是空串，而空串不是一个 agent 的名字）。
fn producer_is_viewer(f: &RowFacts, v: &Viewer) -> bool {
    !f.source_agent.is_empty() && f.source_agent == v.agent_id
}

/// scope 全等：kind 与 key 都比。`viewer.scope == None` ⇒ false（"没声明 scope"不是"在某个 scope 里"）。
fn scope_eq(viewer: Option<&Scope>, owner: &Scope) -> bool {
    match viewer {
        Some(s) => s.kind == owner.kind && s.key == owner.key,
        None => false,
    }
}

/// principal 相等**且两边都非空**：`None` 与 `""` 都不构成身份。
fn principal_eq(viewer: Option<&String>, owner: Option<&String>) -> bool {
    match (viewer, owner) {
        (Some(a), Some(b)) => !a.is_empty() && !b.is_empty() && a == b,
        _ => false,
    }
}

/// 表存在性（不是版本号）：`MEMORY_VERSION` 不 bump 的代价就是读侧必须自己问一句。
fn table_exists(conn: &rusqlite::Connection, name: &str) -> Result<bool, MemError> {
    let n: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
        rusqlite::params![name],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// 装载**有效** grant：capability 按面取、`data_class='memory'`、未撤回、未过期、
/// target 命中（agent 相等，或两边 principal 非空且相等）。scope 留到逐行判。
/// 这一层滤掉的都不再出现在 `decide` 里——**过期即失效由读时判定承担，没有后台清扫**（§5）。
fn load_grants(
    conn: &rusqlite::Connection,
    v: &Viewer,
    now_ms: i64,
) -> Result<Vec<Grant>, MemError> {
    let capability = match v.surface {
        Surface::Agent => "recall",
        Surface::Human => "export",
        // `load` 已经把 Maintenance 短路掉了；真走到这里也只可能"没有可用的能力名"。
        Surface::Maintenance => return Ok(Vec::new()),
    };
    let mut st = conn.prepare(
        "SELECT to_agent_id, to_principal_id, scope_kind, scope_key
           FROM resource_grant
          WHERE capability=?1 AND data_class='memory' AND revoked_at_ms IS NULL
            AND (expires_at_ms IS NULL OR expires_at_ms > ?2)")?;
    let rows = st.query_map(rusqlite::params![capability, now_ms], |r| {
        Ok((
            r.get::<_, Option<String>>(0)?, r.get::<_, Option<String>>(1)?,
            r.get::<_, String>(2)?, r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (to_agent, to_principal, kind, key) = row?;
        let by_agent = to_agent.as_deref() == Some(v.agent_id.as_str());
        let by_principal = principal_eq(to_principal.as_ref(), v.principal_id.as_ref());
        if !(by_agent || by_principal) {
            continue;
        }
        out.push(Grant {
            // `global` 必须在归一两列之前认：`normalize_scope_kind` 会把 "global" 折成 project
            // （它服务的是**行 scope** 那套词表，global 不在里面）。
            global: kind == "global",
            scope_kind: owner::normalize_scope_kind(&kind),
            scope_key: key,
        });
    }
    Ok(out)
}

/// 过闸后的事实行。`viewer == None` ⇒ 原样返回（不过闸：中心 CLI/测试/旧调用）。
/// 装载**一次**：调用方已经拿到了全量事实，逐行重新装载会把"一次全表扫描"变成 N 次。
pub fn facts_visible(
    m: &MemoryHandle,
    viewer: Option<&Viewer>,
    now_ms: i64,
    facts: Vec<RowFacts>,
) -> Result<Vec<RowFacts>, MemError> {
    let Some(v) = viewer else { return Ok(facts) };
    Ok(GateIndex::load(m, v, now_ms)?.filter(facts))
}

/// 按句柄取一行并过闸。**被拒与查不到同形**（都 `Ok(None)`）：调用方因此不必、
/// 也不许把"无权限"渲染成一个与"不存在"不同的回答。
pub fn lookup_visible(
    m: &MemoryHandle,
    viewer: Option<&Viewer>,
    now_ms: i64,
    key: &MemKey,
) -> Result<Option<RowFacts>, MemError> {
    let Some(v) = viewer else { return reader::lookup(m, key) };
    let Some(f) = reader::lookup(m, key)? else { return Ok(None) };
    let gate = GateIndex::load(m, v, now_ms)?;
    if gate.decide(&f).is_ok() {
        Ok(Some(f))
    } else {
        Ok(None)
    }
}
