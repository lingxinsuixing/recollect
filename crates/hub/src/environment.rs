//! W24c：`hub environment declare` —— 运维直陈的**环境属性**（P1-8）。
//!
//! 缺的从来不是写入口：`rc_memory::ensure_environment_anchor` 与 `put_attribute_rows` 从 W14c
//! 起就是 `attribute` 表的唯一生产者，但生产调用点只有一个——导入时的 v2/v3 环境声明
//! （`hub/src/ingest.rs` 的 `write_declared_environment`）。现网是 v1 导入 ⇒ 那张表 0 行，
//! 于是 W14c 裁定的"账号/主机/品牌走环境属性"在库里一条也读不到（W14c/W17 的身份说明
//! 拿不到任何环境格）。本片把那对产品函数接到运维面，**不复制任何判据**：
//!
//! - 锚：`kind='object'`、`canonical=主机名`、id 走 `compute_mem_id` 的内容寻址；
//! - 属性：六格键表（`ATTR_*` 常量）由 [`rc_memory::attribute_role`] 校验，id 内容寻址；
//! - 幂等与替代：同值重跑只增 `identical`，值变化走既有的 supersede 形状（`writers_attr` 自带）。
//!
//! 三条口径在这一层定死（导入侧的同名判据在 `ingest.rs`，两处**不是**两套真值：
//! 归属解析只在 [`latest_claim_principal`] 一处，写入只经那一对产品函数）：
//! 1. **归属必须先有主**：`--principal` 显式给了就用它（但 `unknown` 不算——那是"不知道"，
//!    不是一个人）；缺席时从转录**最新一条** claim 解析，解析不到（没有章、或最新那条答
//!    `unknown`）⇒ [`DeclareError::NoPrincipal`]，调用方退出 3。
//!    **不许越过最新那条去拿更早的已知章**——那是猜，而且会让"Agent 刚说不知道"这件事静默失效。
//! 2. **设备键同时是属性值**：`BatchCtx.device_id`（缺省 `local-qoder`）既进批次三元组，也进
//!    `device_id` 那一条属性——这与导入路径"设备标识取批次键、不取环境变量"逐字同源。
//! 3. **不写 provenance**：与 `writers_attr` 的文件头同一条理由——属性行不是从某一轮话里
//!    抽出来的，出处由 `mode='stated'` + 批次三元组承载。
use rc_ledger::transcript::principal_claims;
use rc_memory::owner::OwnerCtx;
use rc_memory::writers_attr::{self, AttrWriteOutcome};
use rc_memory::writers_v4::principal_is_known;
use rc_memory::{BatchCtx, Scope, ScopeKind};
use rc_store::Root;

/// 一次声明的输入。**与 clap 解耦**（用例直接构造它，不必经过进程参数）；`attributes` 只装
/// 可选的四格（品牌 / OS 账号 / B 站昵称 / Qoder 用户名），主机名与设备标识由 [`declare`]
/// 自己按常量补——"主机名既是锚名也是属性"这条不变量因此只有一处拼写。
pub struct DeclareRequest {
    pub hostname: String,
    pub attributes: Vec<(&'static str, String)>,
    pub principal: Option<String>,
    pub source_agent: String,
    pub device_id: String,
    pub scope_kind: ScopeKind,
    pub scope_key: String,
}

/// 一次声明的读数：锚 id + 那次写属性的四格动作数（键名照 [`AttrWriteOutcome`]）。
#[derive(Debug)]
pub struct Declared {
    pub anchor_id: String,
    pub outcome: AttrWriteOutcome,
}

/// 声明失败的**类别**（不载荷、不带路径；调用方按类别决定退出码与那一行运维话）。
#[derive(Debug)]
pub enum DeclareError {
    /// 键不在六格闭集里。CLI 的旗标面本身就是闭集，这一支是给"未来第二个调用者"的守卫
    /// （`writers_attr::validate` 对同一件事有第二道）。
    UnknownKey,
    /// 必填格空白（主机名 / 设备键 / 署名 / scope 键 / 显式 principal / 某个属性值）。
    /// 空白不是值：与导入侧 `EnvironmentDecl::from_lookup` 的"空白当作没给"同一条取舍。
    /// 只有类别、不带字段名：调用方交出去的是一句静态用法串（零插值、零路径）。
    Blank,
    /// 解析不出一个**已知**主人：没有章、最新那条章答 `unknown`，或显式给的 id 就是 `unknown`。
    NoPrincipal,
    /// 读转录（章表）失败。与 [`Self::Memory`] 一样只留类别：hub 的运维面不印原串。
    Store,
    /// 写记忆库失败（锚或属性）。
    Memory,
}

/// 把一次声明落成"一个环境锚 + 它身上的属性行"。
///
/// 校验（键闭集 / 空白 / scope）排在开事务之前：一条坏 ⇒ 库零行（与 `put_attribute_rows`
/// 同一条尺度）。`now_ms` 是本次运行的钟：新行的 `valid_from_ms`/`created_at_ms` 与旧行的
/// `valid_to_ms` 都取它，一批属性因此带同一个时刻。
pub fn declare(root: &Root, req: &DeclareRequest, now_ms: i64) -> Result<Declared, DeclareError> {
    let hostname = req.hostname.trim();
    if hostname.is_empty() {
        return Err(DeclareError::Blank);
    }
    let scope_key = req.scope_key.trim();
    if scope_key.is_empty() {
        return Err(DeclareError::Blank);
    }
    let source_agent = req.source_agent.trim();
    if source_agent.is_empty() {
        return Err(DeclareError::Blank);
    }
    let device_id = req.device_id.trim();
    if device_id.is_empty() {
        return Err(DeclareError::Blank);
    }
    for (key, value) in &req.attributes {
        if rc_memory::attribute_role(key).is_none() {
            return Err(DeclareError::UnknownKey);
        }
        if value.trim().is_empty() {
            return Err(DeclareError::Blank);
        }
    }
    // 归属必须先有主（W14b 的硬规则）：显式给的 id 也要过 `principal_is_known` 这一把尺子——
    // `--principal unknown` 与"最新章答不知道"是同一件事，写进去就是给环境锚挂一个假主人。
    let principal = match &req.principal {
        Some(given) => {
            let given = given.trim();
            if given.is_empty() {
                return Err(DeclareError::Blank);
            }
            if !principal_is_known(given) {
                return Err(DeclareError::NoPrincipal);
            }
            given.to_string()
        }
        None => latest_claim_principal(root)?,
    };
    // 归属：主人是解析出来的那一格；署名是批次来源；可见性取最保守的一档（与会话缺省
    // `private_agent` 同词），保留口径 `default`——这三格在运维面上没有第二个来源。
    let ctx = BatchCtx {
        source_agent: source_agent.to_string(),
        device_id: device_id.to_string(),
    };
    let owner = OwnerCtx {
        owner_principal_id: Some(principal),
        owner_agent_id: source_agent.to_string(),
        visibility: rc_memory::owner::Visibility::PrivateAgent,
        retention: "default".to_string(),
    };
    let scope = Scope { kind: req.scope_kind, key: scope_key.to_string() };
    // 顺序固定：主机名（锚名兼属性）→ 四个可选项（调用方给的顺序）→ 设备标识。
    // 与导入侧 `write_declared_environment` 的拼法同序：同一组输入两次声明写出的行序一致。
    let mut items: Vec<(&'static str, String)> =
        Vec::with_capacity(req.attributes.len() + 2);
    items.push((rc_memory::ATTR_HOSTNAME, hostname.to_string()));
    items.extend(req.attributes.iter().cloned());
    items.push((rc_memory::ATTR_DEVICE_ID, device_id.to_string()));
    let anchor_id = writers_attr::ensure_environment_anchor(
        root.memory(),
        hostname,
        &ctx,
        &owner,
        &scope,
        now_ms,
    )
    .map_err(|_| DeclareError::Memory)?;
    let outcome = writers_attr::put_attribute_rows(
        root.memory(),
        &anchor_id,
        &items,
        &scope,
        &ctx,
        now_ms,
    )
    .map_err(|_| DeclareError::Memory)?;
    Ok(Declared { anchor_id, outcome })
}

/// 转录里**最新**一条 claim 的 `principal_id`，且它必须说得出主人（`unknown` 不算）。
///
/// "最新"取 `(claimed_at_ms, id)` 的最大值：`claim_on` 保证同一会话的后写那条 `claimed_at_ms`
/// 更大并 `supersedes` 前一条，所以全表最大就是"最近一次盖章"。排序用 `principal_claims`
/// 的既有行解析（不写第二份 SQL）。
///
/// **最新那条是 `unknown` 也算解析不到**：那是 Agent 明确答的"不知道"，越过它去拿更早的
/// 已知章就是把旧结论冒充成现行结论——正是"不猜"这条要挡的形状。
fn latest_claim_principal(root: &Root) -> Result<String, DeclareError> {
    let claims = principal_claims(root.transcript(), None).map_err(|_| DeclareError::Store)?;
    match claims.iter().max_by_key(|c| (c.claimed_at_ms, c.id)) {
        Some(claim) if principal_is_known(&claim.principal_id) => Ok(claim.principal_id.clone()),
        _ => Err(DeclareError::NoPrincipal),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_700_000_000_000;

    fn request(attributes: Vec<(&'static str, String)>) -> DeclareRequest {
        DeclareRequest {
            hostname: "decl-test-host".into(),
            attributes,
            principal: Some("示例主人".into()),
            source_agent: "operator".into(),
            device_id: "local-qoder".into(),
            scope_kind: ScopeKind::Project,
            scope_key: "recollect".into(),
        }
    }

    /// 六格闭集之外 ⇒ [`DeclareError::UnknownKey`]，且库零行（校验排在事务之前）。
    /// 这一支在 CLI 上够不到（旗标面本身就是闭集），所以在函数面上钉住它。
    #[test]
    fn an_unknown_attribute_key_is_rejected_before_any_write() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let req = request(vec![("favourite_colour", "blue".to_string())]);
        match declare(&root, &req, NOW) {
            Err(DeclareError::UnknownKey) => {}
            other => panic!("未知键必须是 UnknownKey，实测：{other:?}"),
        }
        let rows: i64 = root
            .memory()
            .conn()
            .query_row("SELECT count(*) FROM attribute", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "校验失败 ⇒ 一行都不写");
        let anchors: i64 = root
            .memory()
            .conn()
            .query_row("SELECT count(*) FROM anchor WHERE kind='object'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(anchors, 0, "校验失败 ⇒ 环境锚也不建");
    }

    /// 空白主机名不是值 ⇒ 用法错误（`Blank`），不是"用一个空串当锚名"。
    #[test]
    fn a_blank_hostname_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let mut req = request(Vec::new());
        req.hostname = "   ".into();
        match declare(&root, &req, NOW) {
            Err(DeclareError::Blank) => {}
            other => panic!("空白主机名必须是 Blank，实测：{other:?}"),
        }
    }
}
