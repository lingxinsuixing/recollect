//! S2：锚是**导航**，不是记忆卡片。
//!
//! 三个读者面必须分开看：
//! - `view=brief` 的**查询臂**：锚不进候选（否则锚行套进记忆模板、还吃掉注入预算）；
//! - `view=json` 与 `handles=[anchor:…]`：锚**照旧可见**——显式寻址是能力，不是泄漏；
//! - 注入面 `probe_view`：候选里没有锚，但 `rank` 自己仍看得见它们（过滤在候选之后、判定之前，
//!   不是把锚从检索层里删掉——G6 判据③"低分项仍可 recall"的同一条纪律）。
//!
//! 为什么红点只押在**模板断言**（`你记得一条关于`）与**候选断言**（forms 里有没有那张卡）上：
//! 修复前 brief 的页脚也会渲出锚句柄，所以句柄断言「天然为真还是为假」取决于渲染细节；
//! 而"锚被套进记忆模板"与"锚占满 3 行把卡挤掉"是这条缺陷本身的两个症状，与渲染细节无关。

use rc_agent::recall::{recall, recall_narrative, NarrativeView, RecallFilter, RecallRequest};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_state::{put_state_rows, StateWriteItem};
use rc_memory::writers_x::BatchCtx;
use rc_memory::{
    put_card, put_entity, AnchorRole, EntityContent, FactContent, MemKey, Provenance, Scope,
    ScopeKind,
};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
/// 合成主体名：ASCII 单词，`name_occurs` 的词边界与 `rank` 的分词都按"整个词"命中。
/// 不用真实人名——"张三能不能被查询臂点到"取决于分词器，用例不为它赌。
const SUBJECT: &str = "statequarry";

/// 一张真卡 + 三个同名前缀的锚。锚名与卡标题共享 `ubuntu209` 这个 token，
/// 于是**同一次查询**同时命中两者——这正是缺陷现场（实体排在卡前面、把注入额度吃光）。
fn seed(root: &Root) -> (String, Vec<String>) {
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-anchor-case".into() };
    let card = put_card(
        m,
        &FactContent {
            kind: "decision".into(),
            title: "ubuntu209 的口径".into(),
            body: "这是一张真卡。".into(),
            scope: scope.clone(),
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    let mut anchors = Vec::new();
    for name in ["ubuntu209", "ubuntu209-prod", "ubuntu209-ops"] {
        let k = put_entity(
            m,
            &EntityContent {
                kind: "place".into(),
                title: name.into(),
                scope: scope.clone(),
                aliases: vec![name.into()],
            },
            &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
            NOW,
        )
        .unwrap();
        anchors.push(k.key_string());
    }
    rc_index::rebuild(m, NOW).unwrap();
    (card.key_string(), anchors)
}

#[test]
fn brief_query_does_not_render_anchor_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let (card, anchors) = seed(&root);
    let text = recall_narrative(
        root.memory(),
        &RecallRequest::Query("ubuntu209".into()),
        &RecallFilter::empty(),
        NOW,
        NarrativeView::Brief,
    )
    .unwrap()
    .text();
    // ① 主红点（修复前必红，也是 R1 点名的那一格）：锚行被套进了记忆卡片模板。
    assert!(!text.contains("你记得一条关于"), "锚被套进记忆模板：{text}");
    // ② 句柄断言：修复前 brief 的页脚也会渲锚句柄，所以这一条同样是"修复前必红"的读数；
    //    修复后它断的是"锚行整条都不在 brief 里"，不含糊。
    for a in &anchors {
        assert!(!text.contains(a), "锚行 {a} 进了 brief：{text}");
    }
    // ③ 阳性（同一条行为的另一半）：过滤没有把候选集清空——那张真卡必须还在。
    //    少了这一格，"把 brief 的查询臂整条删掉"也能让①②变绿。
    assert!(text.contains("ubuntu209 的口径"), "真卡被连带滤掉了：{text}");
    assert!(text.contains(&card), "真卡的句柄必须还在页脚：{text}");

    // ④ 显式 handles 仍可寻址锚（能力不丢）。
    let key = MemKey::parse(&anchors[0]).unwrap();
    let json = recall(
        root.memory(),
        &RecallRequest::Handles(vec![key]),
        &RecallFilter::empty(),
        NOW,
    )
    .unwrap();
    assert!(json.text().contains(&anchors[0]), "显式锚句柄丢了：{json:?}");
}

/// 阴性/正对的另一半：**被排除的只有 brief 的查询臂**。
/// `view=json` 的查询臂与句柄臂都保留锚——显式寻址与程序面读数不是"注入候选"，
/// 一刀切会把"从锚取回导航"这条能力一起砍掉（那是另一条回归）。
#[test]
fn anchor_在_json_与显式句柄面照旧可见() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let (_card, anchors) = seed(&root);

    // 查询臂 · JSON 面：三个锚都在。这一格修复前也绿（能力本来就丢不了），
    // 它的作用是**钉住修复不许越界**——把过滤写进 `gather` 的公共路径就会当场红在这里。
    let json = recall(
        root.memory(),
        &RecallRequest::Query("ubuntu209".into()),
        &RecallFilter::empty(),
        NOW,
    )
    .unwrap();
    let text = json.text();
    for a in &anchors {
        assert!(text.contains(a), "JSON 查询面丢了锚 {a}：{text}");
    }

    // 句柄臂 · brief 面：显式点名的锚照旧渲染（模板在这里是**允许**的，
    // 因为读的人已经指名要这一行，不是它自己混进候选池）。
    let key = MemKey::parse(&anchors[0]).unwrap();
    let named = recall_narrative(
        root.memory(),
        &RecallRequest::Handles(vec![key]),
        &RecallFilter::empty(),
        NOW,
        NarrativeView::Brief,
    )
    .unwrap()
    .text();
    assert!(named.contains("你记得一条关于"), "显式寻址的锚没有渲染出来：{named}");
    assert!(named.contains(&anchors[0]), "显式寻址的锚句柄不在页脚：{named}");
}

/// 阴性对照：**只命中锚**的查询在 brief 面就是"没有可列出的记忆"（`no_rows`），
/// 而不是把导航行硬塞进记忆模板。同一个查询在 JSON 面上照旧有行——
/// 两条臂上的两种答案正是"brief 不渲染锚"与"锚仍可寻址"的分界，缺一边就分不出来。
#[test]
fn brief_只命中锚时是_no_rows_而不是锚行() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-anchor-case".into() };
    put_entity(
        m,
        &EntityContent {
            kind: "place".into(),
            title: "孤锚导航词".into(),
            scope,
            aliases: vec!["孤锚导航词".into()],
        },
        &[Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    rc_index::rebuild(m, NOW).unwrap();

    let err = recall_narrative(
        m,
        &RecallRequest::Query("孤锚导航词".into()),
        &RecallFilter::empty(),
        NOW,
        NarrativeView::Brief,
    )
    .unwrap_err();
    assert_eq!(err.code, "no_rows", "brief 面对只命中锚的查询报的不是空读数：{err}");
    // JSON 面同一个查询：锚照旧是行（读锚的合法入口之一）。
    let json = recall(
        m,
        &RecallRequest::Query("孤锚导航词".into()),
        &RecallFilter::empty(),
        NOW,
    )
    .unwrap();
    assert!(json.text().contains("anchor:"), "JSON 面也把锚滤掉了：{json:?}");
}

/// 修复轮 1（评审 Critical-1）：查询**只命中锚**、而那条锚上有当前状态时，brief 仍要可读——
/// 状态块端上来，锚行仍不许冒充记忆。
///
/// 这个形状为什么可达：`ROW_TABLES` 只有 Card/Anchor，状态行不进 `rank`；于是
/// "库里只有某实体和它的当前状态、没有同词卡"会让 `gather` 的 `rows`/`absent` 全空。
/// 修复前 `empty_readout` 在那里就报 `no_rows`（本测试在 `.unwrap()` 处即红），
/// `states_for_query` 一次都跑不到——"问某人现在怎样"读不到库里现成的那条状态，
/// 而 spec §5.1-3 要的正是实体导航不丢。
///
/// 三条读数成套：
/// ① 状态块在（主红点，修复前必红）；
/// ② 锚仍不冒充记忆（S2 的契约没被这一步推翻）；
/// ③ 阴性对照：同一个库里问一个不点到任何人的词，仍报 `no_rows`（否则"把判空整条删掉"也能绿）。
/// 状态块形状的夹具：一条实体锚（`rank` 里唯一的命中行，也是那条状态的主体）+
/// 一条**走生产写入口**（`put_state_rows`）落下的当前状态，不手搓 SQL。
/// 两个用例共用它：一个断"状态块被端上来"，一个断"表头不说假话"。
fn state_only_fixture() -> (tempfile::TempDir, Root, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "proj-state-only".into() };
    let anchor = put_entity(
        m,
        &EntityContent {
            kind: "person".into(),
            title: SUBJECT.into(),
            scope: scope.clone(),
            aliases: vec![SUBJECT.into()],
        },
        &[Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    // 归属行也由它写，读闸那条路径才不会因为"缺归属"而另有答案
    // （`crates/rc-agent/tests/views.rs::state` 是同一种播种法）。
    put_state_rows(
        m,
        &[StateWriteItem {
            subject_anchor_id: anchor.id.clone(),
            dimension: "emotion".into(),
            value_after: "焦虑得很".into(),
            mode: "expressed".into(),
            text: format!("{SUBJECT}：情绪为「焦虑得很」。"),
            evidence: vec![("b".repeat(64), AnchorRole::Human)],
            scope: scope.clone(),
            source_ms: NOW,
        }],
        &BatchCtx { source_agent: "agent-state".into(), device_id: "dev-state".into() },
        &OwnerCtx {
            owner_principal_id: None,
            owner_agent_id: "agent-state".into(),
            visibility: Visibility::Project,
            retention: "default".into(),
        },
        NOW,
    )
    .unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    (dir, root, anchor.key_string())
}

#[test]
fn brief_只命中锚但主体有当前状态时端上状态块() {
    let (_dir, root, anchor) = state_only_fixture();
    let m = root.memory();

    // 前提取证（不是判据）：rank 里只有那条锚，没有卡片 ⇒ 这一格确实是"只命中锚"。
    let ranked = rc_search::rank::rank(m, None, NOW, SUBJECT).unwrap();
    assert_eq!(ranked.len(), 1, "夹具前提变了：应当只命中那条锚，实到 {ranked:?}");
    assert!(
        ranked.iter().all(|s| s.key.table == rc_memory::MemTable::Anchor),
        "夹具前提：rank 里出现了非锚行 {ranked:?}"
    );

    let text = recall_narrative(
        m,
        &RecallRequest::Query(SUBJECT.into()),
        &RecallFilter::empty(),
        NOW,
        NarrativeView::Brief,
    )
    .unwrap()
    .text();
    // ① 状态块必须端上来（修复前这里返回 `no_rows`，上面的 `.unwrap()` 当场红）。
    assert!(
        text.contains("【与本次查询点到的主体有关的当前状态】"),
        "查询点到的主体有当前状态，brief 却没端上来：{text}"
    );
    assert!(text.contains("焦虑得很"), "状态块里没有那条状态：{text}");
    // ①b 状态块算**内容**：这一面不许同时宣告"没有可以在预算内列出的条目"（自相矛盾的读数）。
    assert!(
        !text.contains("没有可以在预算内列出的条目"),
        "状态块在场，brief 却宣告一条都没列出：{text}"
    );
    // ② 另一半契约：锚仍然不许套进记忆模板、也不许出现锚句柄。
    assert!(!text.contains("你记得一条关于"), "锚又被套进记忆模板：{text}");
    assert!(!text.contains(&anchor), "锚行进了 brief：{text}");

    // ③ 阴性对照：同一个库里问一个不点到任何人、也命中不了任何行的词 ⇒ 仍是 `no_rows`。
    //    少了这一格，"把 brief 的判空整条删掉"也能让①②绿。
    let err = recall_narrative(
        m,
        &RecallRequest::Query("无关的合成词".into()),
        &RecallFilter::empty(),
        NOW,
        NarrativeView::Brief,
    )
    .unwrap_err();
    assert_eq!(err.code, "no_rows", "无行、无主体、无状态的查询没报空读数：{err}");
}

/// 终审 I-4：表头必须数**实际渲染出来的条目**，状态块只有一块时不许写"共列出 0 条记忆"
/// 再在下面端出内容（同一句自相矛盾，正是这一段修复要杀掉的那种读数）。
///
/// 两半成套：
/// ① 状态块形状：表头里没有 `共列出 0 条记忆`，仍以 `[记忆简报] 截至 ` 开头且带绝对日期；
/// ② 阴性对照（同一条算法在**有行**那一侧的读数）：那个形状下条数就是实际列出的条数，
///    不是 0 —— 少了这一格，"表头恒写一句固定的空话"也能让①绿。
#[test]
fn brief_状态块形状的表头不说共列出零条() {
    let (_dir, root, _anchor) = state_only_fixture();
    let m = root.memory();

    let text = recall_narrative(
        m,
        &RecallRequest::Query(SUBJECT.into()),
        &RecallFilter::empty(),
        NOW,
        NarrativeView::Brief,
    )
    .unwrap()
    .text();
    let header = text.lines().next().unwrap_or_default();
    assert!(header.starts_with("[记忆简报] 截至 "), "表头形状变了：{header}");
    assert!(
        !header.contains("共列出 0 条记忆"),
        "状态块在下面，表头却说零条记忆：{header}"
    );
    assert!(
        text.contains("【与本次查询点到的主体有关的当前状态】"),
        "这一形状本该有状态块，表头断言的对照没了：{text}"
    );

    // ② 有行的形状（`narrative.rs::默认_brief_有头部…` 钉的是形状，这里钉的是**数**）：
    // 同一个夹具里再落一张同词卡，表头必须报 1，而不是继续空话。
    put_card(
        m,
        &FactContent {
            kind: "note".into(),
            title: format!("{SUBJECT} 的第一张卡"),
            body: "同词卡进候选。".into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-state-only".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "c".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let with_rows = recall_narrative(
        m,
        &RecallRequest::Query(SUBJECT.into()),
        &RecallFilter::empty(),
        NOW,
        NarrativeView::Brief,
    )
    .unwrap()
    .text();
    let header = with_rows.lines().next().unwrap_or_default();
    assert!(
        header.contains("共列出 1 条记忆"),
        "有条目时表头必须数实际列出的条数：{header}"
    );
}

/// 注入面：候选里没有锚，但 `rank` 自己仍看得见它们。
/// 红点是两条**成对**的读数：
/// - 阴性：`forms` 里一个锚都不许有（修复前 3 行全是锚）；
/// - 阳性：那张卡必须在 `forms` 里（修复前 3 行额度被锚占满 ⇒ 卡被 `fit` 挤掉）。
///
/// `candidates` 只统计过滤后的池子，所以它同时给出一条可数的读数：4 命中 − 3 锚 = 1。
#[test]
fn probe_候选里没有锚_而_rank_仍看得见它们() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let (card, anchors) = seed(&root);
    let m = root.memory();

    // 前置取证（不是判据，是"这一格确实在缺陷现场"的前提）：rank 里锚与卡同时在。
    let ranked = rc_search::rank::rank(m, None, NOW, "ubuntu209").unwrap();
    assert_eq!(ranked.len(), 4, "夹具前提变了：rank 未同时命中 1 卡 + 3 锚");
    assert!(ranked.iter().any(|s| s.key.key_string() == card), "rank 里没有那张卡");

    let view = rc_search::gate::probe_view(m, None, "ubuntu209", NOW).unwrap();
    assert!(
        view.forms.iter().all(|d| d.key.table != rc_memory::MemTable::Anchor),
        "锚进了注入候选：{:?}",
        view.forms.iter().map(|d| d.key.key_string()).collect::<Vec<_>>()
    );
    assert!(
        view.forms.iter().any(|d| d.key.key_string() == card),
        "卡被锚挤出了注入块：{:?}",
        view.forms.iter().map(|d| d.key.key_string()).collect::<Vec<_>>()
    );
    // 池子计数 = 过滤后的候选数，不许是 rank 全集（那会把"导航不占额度"又算回额度里）。
    assert_eq!(view.candidates, 1, "candidates 仍在数锚：{}", view.candidates);
    // 而 rank 这一层一个锚都没少——过滤在候选之后、判定之前，锚并没有从检索面消失。
    assert!(
        anchors.iter().all(|a| ranked.iter().any(|s| &s.key.key_string() == a)),
        "锚从 rank 里消失了（过滤挪进了检索层）：{ranked:?}"
    );
}
