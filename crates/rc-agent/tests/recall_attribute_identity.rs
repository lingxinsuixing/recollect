//! W14c：工具面（`recollect_recall` 的 brief 臂）对"账号昵称"的答法。
//!
//! 用户 2026-11-06 的验收句：`agent 查询『甲某随行』，那么能获取的信息是示例主人在 B 站的账户的昵称`。
//! 这一片钉三件事（零网络、零 LLM；原料全用生产的写入口与 `hub` 同一组属性）：
//! 1. 查一个**属性值** ⇒ brief 里有一行身份说明（主人 · 环境 · 角色），而不是返回一个同名的人；
//! 2. **现网还站着 W14a 登记的误捕 person 锚**（canonical 就是这个昵称）时，那一行**照旧出现**
//!    ——这是与派发档"只命中属性才给"的一处有意差异（见 `identity_note_for_query` 的文档）；
//! 3. **没有属性命中 ⇒ 输出与 BASE 逐字节相同**（同内容两个库，一个有属性行、一个没有）。
//!
//! W17 起这一行**过读闸**（与 `states_for_query` 同一把尺子）：读者看不到环境锚或看不到主人锚，
//! 这一条整条不出声；`viewer=None` 的口径逐字不变。下半片钉的正是那四格。
use rc_agent::recall::{
    recall_narrative, NarrativeView, RecallFilter, RecallRequest, Viewer,
};
use rc_agent::{AgentErrorKind, ToolError};
use rc_memory::gate::GateIndex;
use rc_memory::keys::{AnchorRole, MemTable, Scope, ScopeKind};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_v4::ensure_principal_anchor;
use rc_memory::writers_x::{put_batch, put_batch_with_owner, BatchCtx, WriteItem};
use rc_memory::{
    ensure_environment_anchor, put_attribute_rows, Provenance, ATTR_BILIBILI_ACCOUNT, ATTR_BRAND,
};
use rc_store::Root;

const NOW: i64 = 1_700_000_700_000;
const PRINCIPAL: &str = "示例主人";
const HOST: &str = "example-host";
const BILI: &str = "甲某随行";
/// 属性行、卡、环境锚的署名 agent：有权读者就是它。
const AGENT: &str = "qoder";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "recollect".into() }
}

fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some(PRINCIPAL.into()),
        owner_agent_id: AGENT.into(),
        visibility: Visibility::Person,
        retention: "keep".into(),
    }
}

/// 主人锚私有（只有 [`AGENT`] 读得到）：用来造"看得见环境、看不见主人"的那一格。
fn owner_private() -> OwnerCtx {
    OwnerCtx { visibility: Visibility::PrivateAgent, ..owner() }
}

/// 环境锚与环境属性公开：陌生读者读得到这一格（否则"第二格被拒"没有判别力）。
fn owner_public() -> OwnerCtx {
    OwnerCtx { visibility: Visibility::Public, ..owner() }
}

fn ctx() -> BatchCtx {
    BatchCtx { source_agent: AGENT.into(), device_id: "local-qoder".into() }
}

/// 一张**正文里带昵称**的卡：它让"无权读者"有候选可命中（从而能判空读数的分型）。
fn nickname_card(title: &str) -> WriteItem {
    WriteItem {
        table: MemTable::Card,
        kind: "note".into(),
        title: title.into(),
        body: format!("{BILI} 是 B 站上的那个昵称。"),
        scope: scope(),
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }],
    }
}

/// 一个"现网形状"的库：主人锚 + 环境锚 + 账号属性。
fn seeded() -> (tempfile::TempDir, Root) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    ensure_principal_anchor(m, PRINCIPAL, Some(PRINCIPAL), &owner(), &scope(), NOW)
        .unwrap()
        .expect("身份已知 ⇒ 主人锚必建");
    let anchor = ensure_environment_anchor(m, HOST, &ctx(), &owner(), &scope(), NOW).unwrap();
    put_attribute_rows(
        m,
        &anchor,
        &[
            (ATTR_BILIBILI_ACCOUNT, BILI.to_string()),
            (ATTR_BRAND, "Acer".to_string()),
        ],
        &scope(),
        &ctx(),
        NOW,
    )
    .unwrap();
    // 取数走 `rank`（FTS 索引），与其余 recall 用例同一条前置：没建索引时这一臂报
    // `index_not_built`——身份说明不是检索，但它挂在同一条取数路上。
    rc_index::rebuild(m, NOW).unwrap();
    (dir, root)
}

/// `seeded()` 之上再加一张正文含昵称的卡（归属同 [`AGENT`]、私有）。
/// 无权读者命中它、又被读闸拒 ⇒ `content_passed > 0`，空读数分型才有判据。
fn seeded_with_card() -> (tempfile::TempDir, Root) {
    let (dir, root) = seeded();
    put_batch(root.memory(), &[nickname_card("昵称卡")], &ctx(), NOW).unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
    (dir, root)
}

/// 主人锚**私有**、环境锚与那张卡**公开**：陌生 agent 看得见"有内容"，但第二格被拒。
/// 返回 `(dir, root, 环境锚, 主人锚)`——两格 id 给前置读数用（经 `GateIndex`，不另造尺子）。
fn seeded_env_only() -> (tempfile::TempDir, Root, String, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let principal =
        ensure_principal_anchor(m, PRINCIPAL, Some(PRINCIPAL), &owner_private(), &scope(), NOW)
            .unwrap()
            .expect("身份已知 ⇒ 主人锚必建");
    let anchor = ensure_environment_anchor(m, HOST, &ctx(), &owner_public(), &scope(), NOW).unwrap();
    put_attribute_rows(
        m,
        &anchor,
        &[(ATTR_BILIBILI_ACCOUNT, BILI.to_string())],
        &scope(),
        &ctx(),
        NOW,
    )
    .unwrap();
    put_batch_with_owner(
        m,
        &[nickname_card("公开昵称卡")],
        &ctx(),
        &owner_public(),
        NOW,
        &[],
    )
    .unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    (dir, root, anchor, principal.id)
}

fn filter(viewer: Option<Viewer>) -> RecallFilter {
    RecallFilter { viewer, ..Default::default() }
}

fn brief(root: &Root, query: &str, viewer: Option<Viewer>) -> String {
    recall_narrative(
        root.memory(),
        &RecallRequest::Query(query.to_string()),
        &filter(viewer),
        NOW,
        NarrativeView::Brief,
    )
    .expect("brief 这一臂不该沉默")
    .text()
}

fn brief_err(root: &Root, query: &str, viewer: Option<Viewer>) -> ToolError {
    recall_narrative(
        root.memory(),
        &RecallRequest::Query(query.to_string()),
        &filter(viewer),
        NOW,
        NarrativeView::Brief,
    )
    .expect_err("这个身份该得到空读数分型，不该拿到 Ok")
}

/// 断言：brief 里既有那一行身份说明，也没有把昵称渲染成"一条记忆"。
fn assert_identity_line(text: &str) {
    assert!(
        text.contains("甲某随行 是 示例主人 的 B站账号昵称（环境 example-host）"),
        "查询词的身份说明必须在 brief 里：\n{text}"
    );
    assert!(text.contains("【查询词指向的账号属性】"), "它是一块有标题的附加内容：\n{text}");
}

// ── ① 查昵称 ⇒ 答出"谁 · 哪个环境 · 什么角色" ───────────────────────────

#[test]
fn 查账号昵称时_brief_答出主人环境与角色() {
    let (_d, root) = seeded();
    assert_identity_line(&brief(&root, BILI, None));
}

#[test]
fn 一个属性值命中两个环境时_brief_两行都写() {
    let (_d, root) = seeded();
    let m = root.memory();
    let other = Scope { kind: ScopeKind::Project, key: "another-proj".into() };
    let anchor = ensure_environment_anchor(m, "另一台", &ctx(), &owner(), &other, NOW).unwrap();
    put_attribute_rows(
        m, &anchor, &[(ATTR_BILIBILI_ACCOUNT, BILI.to_string())], &other, &ctx(), NOW,
    )
    .unwrap();
    let text = brief(&root, BILI, None);
    assert!(text.contains("（环境 example-host）"), "{text}");
    assert!(text.contains("（环境 另一台）"), "歧义不选一个，两行都写：\n{text}");
}

/// 现网那条误捕锚（W14a 登记、未回溯降级）：`kind='person'`、canonical=昵称、active。
/// 它今天仍然站着；这条用例钉的是"它还站着时，昵称的答案也不许丢"。
#[test]
fn 同名的现网误捕_person_锚还在时身份说明照旧出现() {
    let (_d, root) = seeded();
    root.memory()
        .conn()
        .execute(
            "INSERT INTO anchor(id, kind, canonical, reality_status, world_id, scope_kind,
                                 scope_key, status, profile_text, created_at_ms, updated_at_ms,
                                 norm_version)
             VALUES('misfire-anchor','person','甲某随行','real',NULL,'project','recollect',
                    'active',NULL,1700000000000,1700000000000,1)",
            [],
        )
        .unwrap();
    assert_identity_line(&brief(&root, BILI, None));
}

// ── ② 没有属性命中 ⇒ 与 BASE 逐字节相同 ─────────────────────────────────

/// 两张卡两个库：A 库多一组环境属性，B 库没有。查一个**不命中属性**的词 ⇒ 两份 brief 逐字节相同。
/// 这是"默认行为不变"的字节对照（W14c 只在命中属性时多写一块）。
#[test]
fn 查询词不命中属性时_输出与没有属性行的库逐字节相同() {
    let turn_uid = "e".repeat(64);
    let card = |title: &str, body: &str| WriteItem {
        table: rc_memory::MemTable::Card,
        kind: "note".into(),
        title: title.into(),
        body: body.to_string(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-attr".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: turn_uid.clone(), anchor_role: AnchorRole::Human }],
    };
    let with_attrs = tempfile::tempdir().unwrap();
    let without = tempfile::tempdir().unwrap();
    let a = Root::open(with_attrs.path()).unwrap();
    let b = Root::open(without.path()).unwrap();
    for root in [&a, &b] {
        put_batch(
            root.memory(),
            &[card("属性对比卡", "这一条正文里没有那个昵称。")],
            &ctx(),
            NOW,
        )
        .unwrap();
        rc_index::rebuild(root.memory(), NOW).unwrap();
    }
    // 只给 A 加环境属性（B 保持没有 attribute 行）。
    let anchor = ensure_environment_anchor(a.memory(), HOST, &ctx(), &owner(), &scope(), NOW).unwrap();
    put_attribute_rows(
        a.memory(), &anchor, &[(ATTR_BILIBILI_ACCOUNT, BILI.to_string())], &scope(), &ctx(), NOW,
    )
    .unwrap();

    let text_a = brief(&a, "属性对比卡", None);
    let text_b = brief(&b, "属性对比卡", None);
    assert_eq!(text_a, text_b, "查询词不命中属性 ⇒ 两个库的 brief 必须逐字节相同");
    assert!(!text_a.contains("【查询词指向的账号属性】"), "不命中就不出现那一块：\n{text_a}");
}

// ── ③ W17：身份说明过读闸 ───────────────────────────────────────────────

/// 有权读者（导入该环境的 agent）：照常读到，且与 `viewer=None` 逐字节相同——
/// 闸只**减**不该读的，不改变该读的那一格的任何一个字节。
#[test]
fn 有权读者照常读到身份说明_与不过闸逐字节相同() {
    let (_d, root) = seeded();
    let gated = brief(&root, BILI, Some(Viewer::agent(AGENT)));
    assert_identity_line(&gated);
    assert_eq!(
        gated,
        brief(&root, BILI, None),
        "有权读者与 viewer=None 的 brief 必须逐字节相同"
    );
}

/// 无权读者（无任何归属/授权）：身份说明不出现；查询没有任何可见行 ⇒
/// 读数回到 `authorization:visible_none_under_identity`，与"没有属性行"的孪生库同形。
/// W17 之前这一格是"存在性被读出来、读闸那格被绕过"。
#[test]
fn 无权读者读不到身份说明_读数回到身份沉默() {
    let (_d, with_attrs) = seeded_with_card();
    // 前置（独立读数）：这一份库**本来**有那一行——不过闸时读得到（否则本用例没有判别力）。
    assert_identity_line(&brief(&with_attrs, BILI, None));

    let err = brief_err(&with_attrs, BILI, Some(Viewer::agent("nobody")));
    assert_eq!(
        err.kind,
        AgentErrorKind::Authorization,
        "无权读者该落在 Authorization 这一族：{err:?}"
    );
    assert_eq!(
        err.code, "visible_none_under_identity",
        "身份说明被拒后，这一格必须恢复可报（候选在、可见行零）：{err:?}"
    );
    assert!(err.hint.contains("--agent-id"), "hint 要点名 --agent-id：{err}");

    // 孪生库：同一张卡、**没有属性行** ⇒ 读数逐字同形（这就是"绕过"消失之后的正常形状）。
    let dir = tempfile::tempdir().unwrap();
    let bare = Root::open(dir.path()).unwrap();
    put_batch(bare.memory(), &[nickname_card("昵称卡")], &ctx(), NOW).unwrap();
    rc_index::rebuild(bare.memory(), NOW).unwrap();
    let twin = brief_err(&bare, BILI, Some(Viewer::agent("nobody")));
    assert_eq!(
        (err.kind, err.code, err.hint),
        (twin.kind, twin.code, twin.hint),
        "有属性行与没属性行的两份副本，对无权读者必须给出同一个空读数"
    );
}

/// 只看得到环境、看不到主人：环境锚那格过、主人锚那格被拒 ⇒ **整条跳过**。
/// 不许降级成"未知主人"——那仍然把"这个环境有个主人"这件事说出来了。
#[test]
fn 只看得到环境看不到主人_整条跳过不写未知主人() {
    let (_d, root, env_anchor, principal_anchor) = seeded_env_only();
    let m = root.memory();
    let reader = Viewer::agent("reader").with_scope(scope());

    // 前置（独立读数，同一个装载里两格各判一次）：第一格过、第二格不过。
    let gate = GateIndex::load(m, &reader, NOW).unwrap();
    assert!(gate.allows("anchor", &env_anchor), "公开的环境锚对陌生读者可见");
    assert!(!gate.allows("anchor", &principal_anchor), "私有的主人锚对陌生读者不可见");

    let text = brief(&root, BILI, Some(reader));
    assert!(
        text.contains("公开昵称卡"),
        "那张卡对陌生读者可见（否则本用例对'有内容但整条跳过'没有判别力）：\n{text}"
    );
    assert!(!text.contains("【查询词指向的账号属性】"), "第二格被拒 ⇒ 整条不出现：\n{text}");
    assert!(!text.contains("未知主人"), "不许降级成未知主人（那仍然是出声）：\n{text}");

    // 同一份库里看得见两格的读者照常读到（判据不是"谁都拿不到"）。
    assert_identity_line(&brief(&root, BILI, Some(Viewer::agent(AGENT))));
}

/// `viewer=None`（中心 CLI/测试/旧调用）逐字不变：不装载、不判，照旧写出那一行，
/// 且与"有权读者"的 brief 逐字节相同。
#[test]
fn viewer_为_none_时照旧不过闸_身份说明逐字不变() {
    let (_d, root, _env_anchor, _principal_anchor) = seeded_env_only();
    let text = brief(&root, BILI, None);
    assert_identity_line(&text);
    assert_eq!(
        text,
        brief(&root, BILI, Some(Viewer::agent(AGENT))),
        "None 这一格与有权读者的 brief 逐字节相同"
    );
    // 反证：同一份库里读闸真会拦（否则"None 不过闸"这一句在实现上不成立也测不出来）。
    let blocked = brief(&root, BILI, Some(Viewer::agent("reader").with_scope(scope())));
    assert!(!blocked.contains("【查询词指向的账号属性】"), "{blocked}");
}
