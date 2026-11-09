// crates/mcp/tests/p7_state_view.rs
//! P2 状态行的**读侧端到端**（交接档 §十一 的第 1 件）：状态写进 `state_assertion` 之后，
//! 我（agent）在另一个会话里用 `recollect_recall view=overview` 到底读得到什么。
//!
//! 这一片要钉的是四格，缺一格就是"写进去了但没人能用的记忆"：
//! 1. **主语是名字**，不是 64 位裸 anchor id——我读到的必须是"张三：情绪为焦虑得很"，
//!    否则这句话没有主语，我也没法接着问它；
//! 2. **带出处**（`evidence_turn_uid`）——状态行必须能走回那句原话，不然它与一条编造无异；
//! 3. **进 `brief`**——`brief` 是我默认读的那一段；只在 JSON 数组里出现等于我看不到；
//! 4. **过读闸**——别人的私有状态不得进我的 overview（同函数里 fact 那半边本来就过滤，
//!    状态这一半漏了就是跨 owner 泄露）。
//!
//! 播种是白盒的（走真实写入口 `put_state_rows`，不手搓 SQL 行）：本用例要证的是**读侧**，
//! 写侧的形状由 `rc-memory/tests/writers_state.rs` 钉。

mod common;
use common::{call, init, tool_text, Session, TOK};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_state::{put_state_rows, StateWriteItem};
use rc_memory::writers_x::{put_entity, EntityContent, BatchCtx};
use rc_memory::{AnchorRole, Provenance, Scope, ScopeKind};
use rc_store::Root;
use serde_json::Value;
use std::path::Path;

const NOW: i64 = 1_700_000_000_000;
const DEVICE: &str = "dev-synthetic";
/// 读者身份要与**写缝的合成主语**同名：`common::seed` 经 `BatchCtx::synthetic()` 写 card，
/// 它的 `source_agent` 就是 `synthetic-agent`，行的归属记在它头上。换一个名字来读，
/// 读闸会正确地把"别人的行"挡在外面——那不是实现有问题，是夹具的读者身份不对
/// （这一格踩过一次：先怀疑召回，其实是 `GateIndex::allows` 在按归属判）。
const AGENT: &str = "synthetic-agent";

fn uid(n: u8) -> String {
    format!("{:064x}", n)
}

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() }
}

/// 一条锚 + 两条状态行：一条是 alice 的（该读到），一条改挂别人的私有归属（不该读到）。
///
/// 主语名是形参：召回面的分词只认夹具那把尺子（`common::TOK` 是一个合成词），
/// 真实人名"张三"能不能被查询臂命中取决于分词器，用例不为它赌。
fn seed_state(data: &Path, canonical: &str) -> (String, String) {
    let root = Root::open(data).unwrap();
    let anchor = put_entity(
        root.memory(),
        &EntityContent {
            kind: "person".into(),
            title: canonical.into(),
            scope: scope(),
            aliases: vec![],
        },
        &[Provenance { turn_uid: uid(1), anchor_role: AnchorRole::Human }],
        NOW - 1_000,
    )
    .unwrap()
    .id;
    let ctx = BatchCtx { source_agent: AGENT.into(), device_id: DEVICE.into() };
    let owner = OwnerCtx {
        owner_principal_id: None,
        owner_agent_id: AGENT.into(),
        visibility: Visibility::Project,
        retention: "default".into(),
    };
    let mine = StateWriteItem {
        subject_anchor_id: anchor.clone(),
        dimension: "emotion".into(),
        value_after: "焦虑得很".into(),
        mode: "expressed".into(),
        text: format!("{canonical}：情绪为「焦虑得很」。"),
        evidence: vec![(uid(1), AnchorRole::Human)],
        scope: scope(),
        source_ms: NOW,
    };
    let theirs = StateWriteItem {
        dimension: "trust".into(),
        value_after: "信不过".into(),
        text: format!("{canonical}：信任为「信不过」。"),
        evidence: vec![(uid(2), AnchorRole::Agent)],
        source_ms: NOW + 1,
        ..mine.clone()
    };
    put_state_rows(root.memory(), &[mine], &ctx, &owner, NOW).unwrap();
    put_state_rows(root.memory(), &[theirs], &ctx, &owner, NOW).unwrap();
    let _ids: Vec<String> = {
        let mut st = root
            .memory()
            .conn()
            .prepare("SELECT id FROM state_assertion ORDER BY value_after")
            .unwrap();
        st.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    // `trust` 那条改挂另一个 agent 的私有归属：真实链路里这是**别人设备上的私有状态**。
    // 按维度点行而不是按 id 顺序：中文 `ORDER BY value_after` 的次序不由我指定。
    root.memory()
        .conn()
        .execute(
            "UPDATE resource_owner SET owner_agent_id='agent-other', visibility='private_agent'
              WHERE mem_table='state_assertion' AND mem_id=
                (SELECT id FROM state_assertion WHERE dimension='trust')",
            [],
        )
        .unwrap();
    drop(root);
    rc_index::rebuild(Root::open(data).unwrap().memory(), NOW).unwrap();
    (_ids[0].clone(), _ids[1].clone())
}

fn overview_as(data: &Path, agent: &str) -> Value {
    let mut session = Session::start_with(data, &["--device-id", DEVICE]);
    session.ask(&init(1, agent));
    let got = session.ask(&call(
        2,
        "recollect_recall",
        serde_json::json!({"view": "overview"}),
    ));
    let text = tool_text(&got);
    serde_json::from_str::<Value>(&text)
        .unwrap_or_else(|e| panic!("overview 必须是 JSON：{e} / {text}"))
}

fn overview(data: &Path) -> Value {
    overview_as(data, AGENT)
}

/// 1. 状态行读得到，且**主语是名字**、带出处、`brief` 里说得出这一格。
#[test]
fn overview_shows_current_states_with_a_name_and_a_pointer() {
    let dir = tempfile::tempdir().unwrap();
    let (_mine, _theirs) = seed_state(dir.path(), "张三");
    let view = overview(dir.path());
    let states = view["current_states"].as_array().expect("current_states 必须是数组");
    assert_eq!(
        states.len(),
        1,
        "只该读到 alice 自己那条（别人的私有状态不算数）：{states:?}"
    );
    let one = &states[0];
    assert_eq!(one["dimension"], "emotion");
    assert_eq!(one["value"], "焦虑得很");
    // 这两格是本用例存在的理由：今天的实现只给 `subject_anchor_id`，既没有名字也没有出处。
    assert_eq!(
        one["subject"].as_str(),
        Some("张三"),
        "状态行必须带主语的名字，裸 anchor id 我读不出是谁：{one}"
    );
    assert_eq!(
        one["evidence_turn_uid"].as_str(),
        Some(uid(1).as_str()),
        "状态必须能走回那句原话：{one}"
    );
    assert_eq!(view["counts"]["current_states"], 1);
    let brief = view["brief"].as_str().unwrap_or("");
    assert!(
        brief.contains("张三") && brief.contains("焦虑得很"),
        "brief 是我默认读的那一段，状态不进这里等于我看不到：{brief}"
    );
}

/// 2. 库里没有任何状态行时，读数说"零"而不是含糊过去。
#[test]
fn an_empty_state_table_reads_a_zero() {
    let dir = tempfile::tempdir().unwrap();
    Root::open(dir.path()).unwrap();
    let view = overview(dir.path());
    assert_eq!(view["counts"]["current_states"], 0);
    assert!(view["current_states"].as_array().unwrap().is_empty());
    let brief = view["brief"].as_str().unwrap_or("");
    assert!(!brief.contains("状态"), "没有状态行却在 brief 里报状态：{brief}");
    assert_eq!(view["states_hidden_by_identity"], false, "空表不是身份沉默：{view}");
}

/// 3. 同一座库、换一个 `clientInfo.name` 起会话读：状态行在库里、过 scope，
///    但读闸按归属全拒 ⇒ 零可见必须自证（计数器 + 布尔 + brief 三面一起说）。
///    原身份再读一次是**双向对照**：不许只测一边。
#[test]
fn a_wrong_identity_reading_the_same_library_is_told_the_states_are_hidden() {
    let dir = tempfile::tempdir().unwrap();
    seed_state(dir.path(), "张三");

    let other = overview_as(dir.path(), "another-agent");
    assert_eq!(
        other["counts"]["current_states"], 0,
        "别人的私有状态不该进我的 overview：{other}"
    );
    assert_eq!(
        other["states_hidden_by_identity"], true,
        "库里有状态、我一条看不见时，读数必须自己说出口：{other}"
    );
    let brief = other["brief"].as_str().unwrap_or("");
    assert!(brief.contains("看不到"), "brief 必须点明这份身份看不到状态：{brief}");

    let mine = overview(dir.path());
    assert_eq!(mine["counts"]["current_states"], 1, "{mine}");
    assert_eq!(mine["states_hidden_by_identity"], false, "{mine}");
    assert!(
        !mine["brief"].as_str().unwrap_or("").contains("看不到"),
        "身份正确时不许报身份沉默：{mine}"
    );
}

// ── 召回面：查询点到谁，就把谁的当前状态端上来 ──────────────────────────────────

/// 召回要有可召回的行：借用共享夹具的 card（标题含 `common::TOK` 那个合成词）。
fn seed_recallable(data: &Path) {
    common::seed(data, NOW);
}

fn recall(data: &Path, query: &str, view: &str) -> String {
    let mut session = Session::start_with(data, &["--device-id", DEVICE]);
    session.ask(&init(1, AGENT));
    let got = session.ask(&call(
        2,
        "recollect_recall",
        serde_json::json!({"query": query, "view": view}),
    ));
    let text = tool_text(&got);
    if view == "json" {
        // JSON 面是程序读者的形状：状态块**不该**出现在这里（它有 `view=overview` 那个结构化出口）。
        assert!(
            !text.contains("当前状态"),
            "状态块漏进了程序读者该拿的行形状里：{text}"
        );
    }
    text
}

/// 查询点到张三 ⇒ brief 里有他的活状态；**别人那份私有状态不因点名而漏进来**。
#[test]
fn a_query_that_names_a_subject_brings_his_current_states_into_the_brief() {
    let dir = tempfile::tempdir().unwrap();
    seed_state(dir.path(), TOK);
    seed_recallable(dir.path());
    let text = recall(dir.path(), TOK, "brief");
    assert!(
        text.contains(&format!("{TOK} 的情绪是「焦虑得很」")),
        "brief 没带上被点名主体的当前状态：{text}"
    );
    assert!(
        !text.contains("信不过"),
        "别人的私有状态因为查询点了同一个主体就漏进来了：{text}"
    );
    assert!(text.contains("当前状态"), "状态块的小标题不在：{text}");
}

/// 没点到任何主体 ⇒ 一行状态都不写（也不是"写一行说没有状态"）。
#[test]
fn a_query_that_names_nobody_reports_no_states() {
    let dir = tempfile::tempdir().unwrap();
    // 库里确实有状态行，但主语是查询里没有的名字 ⇒ 不该凭空报状态。
    seed_state(dir.path(), "张三丰不存在");
    seed_recallable(dir.path());
    let text = recall(dir.path(), TOK, "brief");
    assert!(text.contains("记忆简报"), "查询本身没召回行：{text}");
    assert!(!text.contains("当前状态"), "没点名任何人却报了状态：{text}");
}

// ── D10：换值之后，"当前状态"必须跟着翻（消费面的存在理由）────────────────────

/// 写一条**更晚**的同维度异值（D10 §3 第一支：后来者上位），再过一遍真实的 mcp 往返。
///
/// 这一条不许只用 SQL 断言：本片要证的是"他状态变了"这句话到底能不能改变我读到的现在。
/// 只拦不裁的实现里，旧值会一直当"当前状态"端上来（比覆盖更坏——它冻结）。
#[test]
fn a_superseded_value_stops_being_the_current_state() {
    let dir = tempfile::tempdir().unwrap();
    seed_state(dir.path(), "张三");

    let before = overview(dir.path());
    let states = before["current_states"].as_array().unwrap();
    assert_eq!(states.len(), 1, "换值之前只该读到 alice 自己那一条：{states:?}");
    assert_eq!(states[0]["value"], "焦虑得很");
    assert!(before["brief"].as_str().unwrap().contains("焦虑得很"));

    let root = Root::open(dir.path()).unwrap();
    let anchor = root
        .memory()
        .conn()
        .query_row(
            "SELECT subject_anchor_id FROM state_assertion WHERE dimension='emotion'",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap();
    put_state_rows(
        root.memory(),
        &[StateWriteItem {
            subject_anchor_id: anchor,
            dimension: "emotion".into(),
            value_after: "平静得很".into(),
            mode: "expressed".into(),
            text: "张三：情绪为「平静得很」。".into(),
            evidence: vec![(uid(3), AnchorRole::Human)],
            scope: scope(),
            source_ms: NOW + 60_000,
        }],
        &BatchCtx { source_agent: AGENT.into(), device_id: DEVICE.into() },
        &OwnerCtx {
            owner_principal_id: None,
            owner_agent_id: AGENT.into(),
            visibility: Visibility::Project,
            retention: "default".into(),
        },
        NOW,
    )
    .unwrap();
    drop(root);

    let after = overview(dir.path());
    let states = after["current_states"].as_array().unwrap();
    assert_eq!(states.len(), 1, "被替代的旧行不该还当当前值：{states:?}");
    assert_eq!(states[0]["value"], "平静得很", "换值之后读到的必须是新值");
    let brief = after["brief"].as_str().unwrap();
    assert!(brief.contains("平静得很"), "brief 还停在旧值上：{brief}");
    assert!(!brief.contains("焦虑得很"), "已经过期的旧值又回到 brief 里：{brief}");
    assert_eq!(after["counts"]["current_states"], 1);
}

// ── I-5b 补的两格：三个新参数与"状态块"这条通道互不改写 ─────────────────────────
// 下面两条是 I-5b 加的，既有用例一字未动。它们各钉派发档 §1 的一格：
// ① 新参数**只属于召回臂**——三个确定性视图是结构化清单，不是邻域读数的宿主，
//    多给三个参数就多算一套是"两个真相"的形状；
// ② `brief` 的 `extra` 通道在库侧是"〔L2 那一跳〕拼在〔状态块〕前面"，
//    显式要那一层时**状态块不许被挤掉**（它是 P2 的召回面，与 L2 是两格内容）。

/// 同一份库、同一个 `view=overview`，把三个新参数拉满与一个字不给，**内容必须逐字节相同**。
/// 唯一豁免的是 `generated_at_ms` 那一格：它是这次调用的钟（`views.rs` 里直接写 `now_ms`），
/// 两次往返之间必然差几十毫秒，把它算进"变了字节"就是把时钟当内容。
/// 顺带钉第二半：坏 `intent` 在**这一面**也被拒——校验点写在 `call_tool` 的开头，
/// 不在 brief/json 那几支里各抄一遍（错误分型族只允许一个家）。
#[test]
fn 三个新参数不改确定性视图的一个字节_而坏_intent_在同一面也被拒() {
    let dir = tempfile::tempdir().unwrap();
    seed_state(dir.path(), TOK);
    let baseline = {
        let mut v = overview(dir.path());
        v["generated_at_ms"] = serde_json::json!(null);
        v
    };
    let mut session = Session::start_with(dir.path(), &["--device-id", DEVICE]);
    session.ask(&init(1, AGENT));
    for args in [
        serde_json::json!({"view": "overview"}),
        serde_json::json!({"view": "overview", "intent": "when", "depth": 2, "include_edges": true}),
        serde_json::json!({"view": "overview", "intent": "why", "depth": 1, "include_edges": false}),
    ] {
        let got = session.ask(&call(2, "recollect_recall", args.clone()));
        assert_eq!(got["result"]["isError"], serde_json::json!(false),
            "确定性视图带着新参数被拒了：{args} / {got}");
        let mut v: Value = serde_json::from_str(&tool_text(&got))
            .unwrap_or_else(|e| panic!("overview 必须是 JSON：{e} / {got}"));
        v["generated_at_ms"] = serde_json::json!(null);
        assert_eq!(v, baseline, "同一份库、同一个视图，只多三个参数就变了读数：{args}");
    }
    // 坏 intent 在这一面同样被拒（校验点不分视图）。
    let got = session.ask(&call(3, "recollect_recall",
        serde_json::json!({"view": "overview", "intent": "because"})));
    assert_eq!(got["result"]["isError"], serde_json::json!(true), "{got}");
    assert!(tool_text(&got).contains("validation:bad_intent"),
        "闭集外的 intent 在确定性视图那一支也要落 bad_intent：{}", tool_text(&got));
}

/// 三格合一条，各钉派发档 §2 的一支：
/// ① `intent=who|what` **不点层**（库侧与 `auto` 同形，没有新的读原语），所以显式给一个
///    不点层的 intent 时，brief 照旧把**状态块**端上来，形状与默认面相同——这一格防的是
///    "参数在共用臂上漏了一格、把不点层的读数也拖走"；
/// ② 显式点层（`intent=when`）而命中的行都不属于任何经历 ⇒ 整个回执换成 `empty:no_episode`
///    那一句正常读数（`isError=false`），**不是一段假装成功的 brief**——空读数与内容是两格；
/// ③ 默认面（不点层）绝不因"命中 L1 但没有 L2"变成错误，即 §2-B 那条命门在状态这一面前的形状。
#[test]
fn 显式点层与不点层的_两种形状在状态块前各自成立() {
    let dir = tempfile::tempdir().unwrap();
    seed_state(dir.path(), TOK);
    seed_recallable(dir.path());
    let mut session = Session::start_with(dir.path(), &["--device-id", DEVICE]);
    session.ask(&init(1, AGENT));
    let mut ask = |id: i64, extra: serde_json::Value| -> (bool, String) {
        // `extra` 里那几格并进同一次调用（`json!` 造出来的是 Object，可直接 merge）。
        let mut args = serde_json::Map::new();
        args.insert("view".into(), serde_json::json!("brief"));
        args.insert("query".into(), serde_json::json!(TOK));
        for (k, v) in extra.as_object().unwrap() { args.insert(k.clone(), v.clone()); }
        let got = session.ask(&call(id, "recollect_recall", serde_json::Value::Object(args)));
        (got["result"]["isError"].as_bool().unwrap(), tool_text(&got))
    };
    let (is_error, plain) = ask(2, serde_json::json!({}));
    assert!(!is_error, "默认面被标成故障：{plain}");
    assert!(plain.contains("当前状态"), "默认面没有状态块（下面那个等式就成了空的）：{plain}");
    assert!(plain.contains(&format!("{TOK} 的情绪是「焦虑得很」")), "状态内容没了：{plain}");

    let (is_error, who) = ask(3, serde_json::json!({"intent": "who"}));
    assert!(!is_error, "intent=who 不点层，不该失败：{who}");
    assert!(who.contains("当前状态"), "不点层的 intent 把状态块挤掉了：{who}");
    assert!(!who.contains("no_episode") && !who.contains("no_edges"),
        "不点层的一支不许冒邻域那两码：{who}");
    // 两次往返之间只可能差在表头那个"截至…"的分钟上，故只比状态块之后的那一段。
    let cut = |t: &str| t.split_once("当前状态").map(|(_, r)| r.to_string()).unwrap_or_default();
    assert_eq!(cut(&plain), cut(&who),
        "同一份库同一句话、只差一个不点层的参数，读到的现在却不一样：\
         默认面 {} / intent=who {}", cut(&plain), cut(&who));

    let (is_error, when) = ask(4, serde_json::json!({"intent": "when"}));
    assert!(!is_error, "点层而没有那一层是正常读数：{when}");
    assert!(when.contains("empty:no_episode"), "点层而没有那一层要说得出码：{when}");
    assert!(!when.contains("当前状态"), "空读数是一句回执，不是一段 brief：{when}");
}
