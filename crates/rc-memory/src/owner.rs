//! 资源归属的**唯一实现点**（S1b：C2b 吸收推导 / C3 写入继承 / C4 读闸与回填共用）。
//!
//! 三条不变量在这里各占一格，其余模块只调用不复制：
//! ① `visibility` 是隐式受众上界，行 scope 是它的天然上界（[`ceiling`]），两者取更保守的
//!    一档（[`effective`]）——`org` 今天没有身份来源，所以它永远被行 scope 压下来；
//! ② 载荷里的 `scope_kind` 可能来自契约五值（`ingest_v3::ScopeKind`：project|agent|org|
//!    global|session），而 `resource_owner.scope_kind` 只收记忆行词表（person|project|entity，
//!    `keys.rs`）。[`normalize_scope_kind`] 是那唯一一次归一：person/entity 保留，其余一律
//!    project（与 reader 的脏 slug 下界一致）。**不许**为此放宽表的 CHECK；
//! ③ `retention` 在写入点只收 `default`/`keep`/`days:<正整数>`（与契约 `valid_retention`
//!    同义），非法值归一成 `default`——表上那条 `LIKE 'days:%'` 只是形状兜底。
//!
//! `insert` 是 `INSERT OR IGNORE`，"先写者赢"：已有归属行**永不覆盖**。调用方拿到 `false`
//! 之后自己用 [`existing_owner`] 判是不是 owner 冲突（S1b 的裁定是逐行抑制 + 记账，不是覆盖）。
use crate::keys::{MemTable, Scope, ScopeKind};
use crate::MemError;
use rc_store::MemoryHandle;
use rusqlite::{params, OptionalExtension};
pub use rc_contract::ingest_v3::Visibility;

/// 行 scope 对可见性的天然上界：person→person，project→project，entity→person。
/// 记忆行是"某个 scope 里的事实"，它的受众不可能比它所在的 scope 更宽。
pub fn ceiling(kind: ScopeKind) -> Visibility {
    match kind {
        ScopeKind::Person => Visibility::Person,
        ScopeKind::Project => Visibility::Project,
        // 实体锚不是项目：它可以被项目共享，但不该因为"锚是公共的"而把行放宽到 project。
        ScopeKind::Entity => Visibility::Person,
    }
}

/// 可见性序：private_agent < person < project < org < public。
/// 数字只在本模块用（`effective` 的取小），不外泄成可比较的类型。
fn rank(v: Visibility) -> u8 {
    match v {
        Visibility::PrivateAgent => 0,
        Visibility::Person => 1,
        Visibility::Project => 2,
        Visibility::Org => 3,
        Visibility::Public => 4,
    }
}

/// 取更保守的一档：`min(session.visibility, ceiling(row.scope.kind))`。
/// 会话声明的可见性只是**上界**，行 scope 再压一次——两者都放宽的那一格才算放宽。
pub fn effective(session: Visibility, kind: ScopeKind) -> Visibility {
    let cap = ceiling(kind);
    if rank(session) <= rank(cap) {
        session
    } else {
        cap
    }
}

/// 载荷 `scope_kind` → 记忆行词表。契约五值里只有 `person` 与记忆行同名，
/// `entity` 是记忆行独有（载荷不会出现，但真出现时保留）；其余（project/agent/org/
/// global/session）与空串、垃圾串一律 project——这是**更宽**的那一档，所以它安全：
/// 上界仍由 [`effective`] 与会话可见性再压一次。
pub fn normalize_scope_kind(raw: &str) -> ScopeKind {
    match raw {
        "person" => ScopeKind::Person,
        "entity" => ScopeKind::Entity,
        _ => ScopeKind::Project,
    }
}

/// `Visibility` 是 serde snake_case 的枚举，没有 `as_str`；落库只许契约字面量
/// （`private_agent`，不是 `PrivateAgent`）——写错一个字母就会被 `resource_owner`
/// 的 CHECK 拒掉，或者更糟：读侧再也认不出这一行。
///
/// `pub`：锚可见性谓词（[`anchor_visible_pred`]）要在 SQL 里比这四个非私有 slug，
/// 字面量仍只从这一处取，不在调用点手写第二套。
pub fn visibility_slug(v: Visibility) -> &'static str {
    match v {
        Visibility::PrivateAgent => "private_agent",
        Visibility::Person => "person",
        Visibility::Project => "project",
        Visibility::Org => "org",
        Visibility::Public => "public",
    }
}

/// `Visibility` 的字面量 → 枚举。**这是唯一实现点**（C3 把 `hub::rows` 的私有副本搬到这里，
/// `select_batch` 的 sessions 读侧用同一个）。认不出（脏值/大小写错/空串）一律
/// `PrivateAgent`：读侧最保守的一档，与 `sessions` 的 NULL 语义（COALESCE 成同一个词）同向。
pub fn parse_visibility(raw: &str) -> Visibility {
    match raw {
        "person" => Visibility::Person,
        "project" => Visibility::Project,
        "org" => Visibility::Org,
        "public" => Visibility::Public,
        _ => Visibility::PrivateAgent,
    }
}

/// retention 的真值校验（与 `rc_contract::ingest_v3` 里那个私有的 `valid_retention` 同义：
/// `default`/`keep`/`days:<u32>` 且大于 0）。非法值归一成 `default`，**不拒收**——
/// 这一层是写入面的兜底，拒收的判定在契约校验里。
/// C3 起它是 `pub`：`OwnerCtx.retention`（来自会话）与 `OwnerRow.retention` 走同一个归一，
/// `select_batch` 读会话时也调它，**不许**长出第二把尺子。
pub fn normalize_retention(raw: &str) -> String {
    if matches!(raw, "default" | "keep") {
        return raw.to_string();
    }
    match raw.strip_prefix("days:") {
        Some(days) if days.parse::<u32>().map(|n| n > 0).unwrap_or(false) => raw.to_string(),
        _ => "default".to_string(),
    }
}

/// 写入侧的归属上下文（S1b-C3）：owner/visibility/retention 的三格来源。
///
/// 为什么是独立形参而不是 `BatchCtx` 的字段：`BatchCtx` 全仓有 20+ 结构体字面量，
/// 加字段会把 C3 变成一次大范围测试改名；且这两个概念的来源不同——`BatchCtx` 是
/// P 独占列（source_agent/device_id），`OwnerCtx` 是**会话**的归属声明。
#[derive(Debug, Clone)]
pub struct OwnerCtx {
    pub owner_principal_id: Option<String>,
    pub owner_agent_id: String,
    pub visibility: Visibility,
    pub retention: String,
}

impl OwnerCtx {
    /// 旧四把单条写/合成夹具的默认：owner=署名 agent，private，default。
    /// 调用方递进来的只有署名（`PStamps.source_agent`）——owner 绝不从内容形状取。
    pub fn synthetic(source_agent: &str) -> OwnerCtx {
        OwnerCtx {
            owner_principal_id: None,
            owner_agent_id: source_agent.to_string(),
            visibility: Visibility::PrivateAgent,
            retention: "default".to_string(),
        }
    }
}

/// 一行归属。字段名与 `resource_owner` 的列一一对应（`scope` 展开成 `scope_kind`+`scope_key`），
/// `visibility`/`scope.kind` 在这里是枚举、落库时才变字面量。
/// 不给 `PartialEq`：`Scope` 没有相等语义（`keys.rs` 故意不 derive），归属的比较面是
/// [`existing_owner`] 那一对 `(owner_principal_id, owner_agent_id)`。
#[derive(Debug, Clone)]
pub struct OwnerRow {
    pub mem_table: String,
    pub mem_id: String,
    pub owner_principal_id: Option<String>,
    pub owner_agent_id: String,
    pub created_by_agent: String,
    pub visibility: Visibility,
    /// 归一后的 retention（`insert` 还会再过一遍 [`normalize_retention`]）。
    pub retention: String,
    pub scope: Scope,
    pub source_agent: Option<String>,
    pub device_id: Option<String>,
    pub created_at_ms: i64,
}

/// 写一行归属：`INSERT OR IGNORE`。返回 `true`=新写、`false`=已有（**不覆盖**）。
/// 冲突交给调用方判（同 owner 的重复是幂等重放，不同 owner 才是 `owner_conflict`）。
pub fn insert(tx: &rusqlite::Transaction<'_>, row: &OwnerRow) -> Result<bool, MemError> {
    let n = tx.execute(
        "INSERT OR IGNORE INTO resource_owner(mem_table, mem_id, owner_principal_id, owner_agent_id,
                                             created_by_agent, visibility, retention, scope_kind,
                                             scope_key, source_agent, device_id, created_at_ms)
         VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            row.mem_table,
            row.mem_id,
            row.owner_principal_id,
            row.owner_agent_id,
            row.created_by_agent,
            visibility_slug(row.visibility),
            normalize_retention(&row.retention),
            row.scope.kind.as_str(),
            row.scope.key,
            row.source_agent,
            row.device_id,
            row.created_at_ms,
        ],
    )?;
    Ok(n == 1)
}

/// 已有归属行的 `(owner_principal_id, owner_agent_id)`；没有行 ⇒ `None`。
/// 只读这两列：判"是不是同一个 owner"不需要 visibility/scope，也不许用它们抹平。
pub fn existing_owner(
    tx: &rusqlite::Transaction<'_>,
    mem_table: &str,
    mem_id: &str,
) -> Result<Option<(Option<String>, String)>, MemError> {
    Ok(tx
        .query_row(
            "SELECT owner_principal_id, owner_agent_id FROM resource_owner
             WHERE mem_table=?1 AND mem_id=?2",
            params![mem_table, mem_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// **锚对某个归属上下文可见**的唯一 SQL 谓词（事实/事件候选池与程序闸共用这一份文本）。
///
/// 规则（fail-closed，三条缺一不可）：
/// ① 锚有 `resource_owner` 行——没有归属证据的锚不进任何候选、也不被任何行引用；
/// ② 该行的 owner 已解决（`owner_agent_id` 非空）；
/// ③ 行的 owner 是读者本人，**或** visibility 非 `private_agent` 且 scope（kind+key）全等。
///
/// 为什么不是 `GateIndex`：那套读闸回答的是"某位读者能否**读**这一行"，还有 grant 与
/// principal 两档；这里回答的是"这个锚能不能作为**这批抽取**的提示/引用"，口径由 §候选池
/// 定义，比读闸窄（不认 grant），所以单独一支——但两支共用 [`visibility_slug`] 与
/// [`normalize_scope_kind`] 的 slug/归一，不各自手写字面量。
///
/// 参数用**命名绑定**（`:anchor_id` / `:viewer_agent` / `:vis_person` / `:vis_project` /
/// `:vis_org` / `:vis_public` / `:viewer_scope_kind` / `:viewer_scope_key`）：同一段文本
/// 既能挂在候选池查询的 `a.id` 上，也能挂在闸门的单锚查询上，不必为两种查询各抄一遍。
/// `anchor_id_expr` 只接受调用点写死的内部表达式（列名或命名占位），不是用户输入。
pub fn anchor_visible_pred(anchor_id_expr: &str) -> String {
    let table = MemTable::Anchor.prefix();
    format!(
        "EXISTS (SELECT 1 FROM resource_owner AS ro \
          WHERE ro.mem_table='{table}' AND ro.mem_id={anchor_id_expr} \
            AND ro.owner_agent_id <> '' \
            AND (ro.owner_agent_id = :viewer_agent \
                 OR (ro.visibility IN (:vis_person, :vis_project, :vis_org, :vis_public) \
                     AND ro.scope_kind = :viewer_scope_kind \
                     AND ro.scope_key = :viewer_scope_key)))"
    )
}

/// [`anchor_visible_pred`] 的单锚版本：一个锚对 `(viewer_agent_id, viewer_scope)` 是否可见。
/// 与候选池查询是同一段谓词文本——两处口径不可能漂。
pub fn anchor_visible(
    m: &MemoryHandle,
    anchor_id: &str,
    viewer_agent_id: &str,
    viewer_scope: &Scope,
) -> Result<bool, MemError> {
    let sql = format!("SELECT {}", anchor_visible_pred(":anchor_id"));
    Ok(m.conn().query_row(
        &sql,
        rusqlite::named_params! {
            ":anchor_id": anchor_id,
            ":viewer_agent": viewer_agent_id,
            ":vis_person": visibility_slug(Visibility::Person),
            ":vis_project": visibility_slug(Visibility::Project),
            ":vis_org": visibility_slug(Visibility::Org),
            ":vis_public": visibility_slug(Visibility::Public),
            ":viewer_scope_kind": viewer_scope.kind.as_str(),
            ":viewer_scope_key": viewer_scope.key,
        },
        |r| r.get(0),
    )?)
}
