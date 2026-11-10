//! 环境/账号属性（`attribute` 表）的**唯一写入口**（W14c）。
//!
//! 这张表从 canonical DDL 起就在、也有复制顺序（`hub/src/rows.rs` 的 `INSERT_ORDER`），
//! 但到 W14c 之前**零生产者**——`rebind.rs` 把它登记成"id 规则未核实 ⇒ 不往它里面发明值"。
//! 本片补的就是生产者与 id 规则，落点是一条用户裁定：
//!
//! > "以上所有信息都归属于示例主人这个实体，不过指定名字是归属于哪个实体环境/设备在使用。"
//!
//! 所以 OS 账号（如 `u`）、B 站昵称（如 `甲某随行`）、主机名、品牌、
//! Qoder 用户名**都不是人**：它们的家是环境锚（`kind='object'`）上的
//! 一行 `attribute`，归属（`resource_owner.owner_principal_id`）仍是主人。
//! 名字只标识"哪个环境在用"，不标识"是谁"——这正是 W14a 把账号形状的 person 锚拒收之后
//! 它们一直没有落点的原因。
//!
//! 三条纪律：
//! 1. **幂等**：id 内容寻址（[`compute_attribute_id`]：锚 · 键 · 归一值 · 归属）。
//!    同 `(锚, 键, 同值)` 重导不新增行（`ON CONFLICT(id) DO NOTHING`）。
//! 2. **值变了就换行并关旧行**：新行插入后，同一 `(锚, 键)` 的其它活行被写上
//!    `valid_to_ms = 本次批次钟` 与 `superseded_by = 新行 id`——与 `state_assertion` 的
//!    替代规则同一形状（读侧只认 `valid_to_ms IS NULL`）。
//!    值又变回旧值时**复用同一格并重开有效期**（id 是内容寻址的，那一行就是"这个值"），
//!    不新增第三行、也不留下一条"当前是 A、但 A 那行是关着的"的静默空档。
//! 3. **不写 provenance**：这张表的行不是从某一轮话里抽出来的，没有 `turn_uid` 可指
//!    （`provenance.turn_uid` 是 NOT NULL）。出处由 `mode='stated'` + 批次三元组
//!    （`source_agent`/`device_id`/`created_at_ms`）承载，不伪造一条轮。
//!
//! 属性行**不单独写 `resource_owner`**：归属由所在环境锚的那一行承载，读侧按锚取主人
//! （[`crate::reader::attribute_owners`]）。给每条属性再抄一份主人，只会让"改主人"变成
//! 六处写点。
use crate::keys::{MemTable, Scope};
use crate::owner::{self, OwnerCtx};
use crate::writers_fact::json_string;
use crate::writers_x::BatchCtx;
use crate::MemError;
use rc_contract::normalize_body;
use rc_store::MemoryHandle;
use rusqlite::params;
use sha2::{Digest, Sha256};

/// 属性键表的六格（写死的常量，不新造枚举表、**不扩 DDL 的 CHECK**——`attribute.key`
/// 是自由 TEXT，扩 CHECK 就是重建全库）。
///
/// 每格给中文角色名：读者问"甲某随行 是谁的什么"时，回答里的"什么"就是它。
/// 写侧（环境变量 → 键）与读侧（键 → 角色名）**共用这一份**，不各写一张表。
pub const ATTRIBUTE_KEYS: [(&str, &str); 6] = [
    (ATTR_HOSTNAME, "主机名"),
    (ATTR_BRAND, "品牌"),
    (ATTR_OS_USER, "操作系统账号"),
    (ATTR_DEVICE_ID, "设备标识"),
    (ATTR_BILIBILI_ACCOUNT, "B站账号昵称"),
    (ATTR_QODER_ACCOUNT, "Qoder 用户名"),
];

/// 主机名。它同时是环境锚的 `canonical`：环境以"被叫的名字"入锚，别的属性挂在它身上。
pub const ATTR_HOSTNAME: &str = "hostname";
/// 品牌（`Acer`）。
pub const ATTR_BRAND: &str = "brand";
/// 操作系统账号（`u`）。
pub const ATTR_OS_USER: &str = "os_user";
/// 设备标识。**值取批次的 `device_id`**，不是环境变量——它是这条链自己用的设备键。
pub const ATTR_DEVICE_ID: &str = "device_id";
/// B 站账号昵称（`甲某随行`）。
pub const ATTR_BILIBILI_ACCOUNT: &str = "bilibili_account";
/// Qoder 用户名（`example-account`）。
pub const ATTR_QODER_ACCOUNT: &str = "qoder_account";

/// 环境锚的 kind。`object` 是 `ANCHOR_KINDS` 里既有的值：环境/设备是"东西"，不是人。
/// 不拿 `person`（那是主人）、不拿 `agent`（那是会话自锚）。
pub const ENV_ANCHOR_KIND: &str = "object";

/// 属性的 `mode`：用户/运维**直陈**（环境变量就是直陈）。闭集三值里的第一值。
pub const ATTR_MODE_STATED: &str = "stated";

/// 属性的 `value_type`：六格全 `string`。
pub const ATTR_VALUE_TYPE_STRING: &str = "string";

/// 键 → 中文角色名；不是六格之一 ⇒ `None`（写侧用它挡拼错的键，读侧用它判"这条命中算什么"）。
pub fn attribute_role(key: &str) -> Option<&'static str> {
    ATTRIBUTE_KEYS.iter().find(|(k, _)| *k == key).map(|(_, role)| *role)
}

/// 属性行的 id = sha256("attribute" · 锚 · 键 · 归一值 · 归属类型 · 归一归属键)，hex。
///
/// **为什么不走 `compute_mem_id`**：那一条的第一个参数是 `MemTable`，而 `keys.rs` 的
/// `MemTable` 只有 `Anchor|Card`（`attribute` 不是可指认句柄，全仓数百处引用都建立在
/// 这两值上）。形状照同文件族的 `compute_state_id`：段间 `0u8` 分隔（`("ab","c")` 与
/// `("a","bc")` 不许撞）、值先过 `normalize_body`（CRLF/NFC 不造"同一个值的两行"）。
///
/// **时间不进 preimage**：`created_at_ms` / `valid_from_ms` 是列值不是身份——
/// 进了它，每次重导都会换一个新 id，"同值重导不新增行"这条幂等就不成立了。
pub fn compute_attribute_id(anchor_id: &str, key: &str, value: &str, scope: &Scope) -> String {
    let normalized_value = normalize_body(value);
    let normalized_key = normalize_body(&scope.key);
    let mut h = Sha256::new();
    for part in [
        "attribute", anchor_id, key, &normalized_value,
        scope.kind.as_str(), &normalized_key,
    ] {
        h.update(part.as_bytes());
        h.update([0u8]);
    }
    let mut s = String::with_capacity(64);
    for b in h.finalize() { s.push_str(&format!("{b:02x}")); }
    s
}

/// 为一次导入环境声明 ensure 一个**环境锚**（`kind='object'`、`canonical=主机名`）。
///
/// 与 [`crate::writers_v4::ensure_agent_anchor`] / `ensure_principal_anchor` 逐条同构：
/// 1. **身份只有一条算法**：`compute_mem_id(Anchor, "object", 主机名, "", scope)` ⇒
///    同 `(canonical, scope)` 落在同一格，`INSERT OR IGNORE` 天然幂等；
/// 2. **既有行不覆盖**：锚的内容与归属都"先写者赢"（`owner::insert` 是 `INSERT OR IGNORE`）；
/// 3. **归属行的 `device_id` 写这台设备**（不是 `NULL`）：`NULL` 是**主人锚**的契约
///    （`writers_v4::session_principal_anchor_id` 靠它把主人锚与正文里的人名锚分开），
///    环境锚本身就是某台设备的锚，写 `NULL` 会把两种锚搅成一格。
pub fn ensure_environment_anchor(
    m: &MemoryHandle,
    canonical: &str,
    ctx: &BatchCtx,
    owner: &OwnerCtx,
    scope: &Scope,
    now_ms: i64,
) -> Result<String, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    let canonical = normalize_body(canonical);
    if canonical.is_empty() {
        return Err(MemError::BadA1Field("canonical"));
    }
    let id = crate::compute_mem_id(MemTable::Anchor, ENV_ANCHOR_KIND, &canonical, "", scope);
    let tx = m.conn().unchecked_transaction()?;
    tx.execute(
        "INSERT OR IGNORE INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                      scope_key, status, profile_text, created_at_ms,
                                      updated_at_ms, norm_version)
         VALUES(?1,?2,?3,'real',NULL,?4,?5,'active',NULL,?6,?6,?7)",
        params![
            id,
            ENV_ANCHOR_KIND,
            canonical,
            scope.kind.as_str(),
            scope.key,
            now_ms,
            rc_store::capability::NORM_VERSION as i64
        ],
    )?;
    owner::insert(
        &tx,
        &owner::OwnerRow {
            mem_table: MemTable::Anchor.prefix().to_string(),
            mem_id: id.clone(),
            owner_principal_id: owner.owner_principal_id.clone(),
            owner_agent_id: owner.owner_agent_id.clone(),
            created_by_agent: ctx.source_agent.clone(),
            visibility: owner::effective(owner.visibility, scope.kind),
            retention: owner::normalize_retention(&owner.retention),
            scope: scope.clone(),
            source_agent: Some(ctx.source_agent.clone()),
            device_id: Some(ctx.device_id.clone()),
            created_at_ms: now_ms,
        },
    )?;
    tx.commit()?;
    Ok(id)
}

/// 一次写属性的读数。四格都是**这次调用真发生的动作数**：重导同值 ⇒ 只有 `identical` 增。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AttrWriteOutcome {
    /// 真插入的行数。
    pub inserted: usize,
    /// 同 `(锚, 键, 值)` 已有活行 ⇒ 不写第二行。
    pub identical: usize,
    /// 值变了 ⇒ 被本次关掉的旧活行数（`created_at_ms` 更晚的那条上位）。
    pub superseded: usize,
    /// 值又变回旧值 ⇒ 重开有效期的行数（它原本是关着的）。
    pub reopened: usize,
}

fn blank(part: &str) -> bool {
    normalize_body(part).is_empty()
}

/// 写侧校验（在任何事务开始之前）：键必须是六格之一、值不许空白、锚 id 不许空白。
/// 一条坏 ⇒ 整批 `Err`，库里零行（与 `put_state_rows` 同一条尺度）。
fn validate(anchor_id: &str, items: &[(&'static str, String)]) -> Result<(), MemError> {
    if blank(anchor_id) {
        return Err(MemError::BadA1Field("anchor_id"));
    }
    for (key, value) in items {
        if attribute_role(key).is_none() {
            return Err(MemError::BadA1Field("key"));
        }
        if blank(value) {
            return Err(MemError::BadA1Field("value_text"));
        }
    }
    Ok(())
}

/// 属性行的唯一插入字面量。`valid_to_ms` / `superseded_by` 一律从 NULL 起步——
/// 关旧行是**旧行**那一格的动作（[`ATTR_CLOSE`]），新行不冒充"已经被替代"。
const ATTR_INSERT: &str = "INSERT INTO attribute(
    id, anchor_id, key, value_json, value_text, value_type, unit, valid_from_ms, valid_to_ms,
    superseded_by, confidence, mode, text, world_id, scope_kind, scope_key, source_agent,
    device_id, created_at_ms, norm_version)
 VALUES(?1,?2,?3,?4,?5,'string',NULL,?6,NULL,NULL,0.8,'stated',?7,NULL,?8,?9,?10,?11,?6,?12)
 ON CONFLICT(id) DO NOTHING";

/// 关掉同一 `(锚, 键)` 的其它活行并指认替代者。`id<>?5` 是并发位兼"别把自己关掉"那道：
/// 另一个写者几乎同时推了同一对值时，后提交那一边命中 0 行，不把已经关过的行再关一次。
const ATTR_CLOSE: &str =
    "UPDATE attribute SET valid_to_ms=?1, superseded_by=?2
      WHERE anchor_id=?3 AND key=?4 AND valid_to_ms IS NULL AND id<>?5";

/// 值变回旧值时重开那一格（内容寻址 ⇒ 同一值就是同一行）。`created_at_ms` / `valid_from_ms`
/// **保持原值**：它们是这一行的身份时刻，不是"这一次又说了"的时刻。
const ATTR_REOPEN: &str =
    "UPDATE attribute SET valid_to_ms=NULL, superseded_by=NULL
      WHERE id=?1 AND valid_to_ms IS NOT NULL";

/// 写一批环境属性。**整批一个事务**：任何一条失败 ⇒ 一条都不写。
///
/// `now_ms` 是批次钟：新行的 `valid_from_ms` 与 `created_at_ms`、旧行的 `valid_to_ms`
/// 都取它（同一批里的属性因此带着同一个时刻，读侧不会看到半批）。
pub fn put_attribute_rows(
    m: &MemoryHandle,
    anchor_id: &str,
    items: &[(&'static str, String)],
    scope: &Scope,
    ctx: &BatchCtx,
    now_ms: i64,
) -> Result<AttrWriteOutcome, MemError> {
    if m.is_read_only() {
        return Err(MemError::ReadOnly);
    }
    validate(anchor_id, items)?;
    let mut out = AttrWriteOutcome::default();
    if items.is_empty() {
        return Ok(out);
    }
    let norm_version = rc_store::capability::NORM_VERSION as i64;
    let tx = m.conn().unchecked_transaction()?;
    for (key, value) in items {
        let id = compute_attribute_id(anchor_id, key, value, scope);
        // `text` 是这一行的消费句：角色名来自键表（唯一来源），值原样。
        let text = format!("{}：{}", attribute_role(key).unwrap_or(key), normalize_body(value));
        let inserted = tx.execute(
            ATTR_INSERT,
            params![
                id,
                anchor_id,
                key,
                json_string(value),
                normalize_body(value),
                now_ms,
                text,
                scope.kind.as_str(),
                scope.key,
                ctx.source_agent,
                ctx.device_id,
                norm_version,
            ],
        )?;
        if inserted > 0 {
            out.inserted += 1;
        } else {
            // 同一格已经在（同值重导，或值变回旧值）。分辨它们：活行 = 重导；关着的行 = 回来。
            let live: i64 = tx.query_row(
                "SELECT count(*) FROM attribute WHERE id=?1 AND valid_to_ms IS NULL",
                params![id],
                |r| r.get(0),
            )?;
            if live > 0 {
                out.identical += 1;
            } else {
                out.reopened += tx.execute(ATTR_REOPEN, params![id])?;
            }
        }
        out.superseded += tx.execute(ATTR_CLOSE, params![now_ms, id, anchor_id, key, id])?;
    }
    tx.commit()?;
    Ok(out)
}
