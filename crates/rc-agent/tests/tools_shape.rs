//! 写入面（`remember` / `vote` / `status`）与 stdout 形状的四条判据（任务 8 Step 5）。
//! 本文件的每一条**照 `task-8b-brief.md` 的 Step 5 落地**，包括它引用的 Step 6 产物
//! （`rc_agent::sinks` / `rc_agent::tools::{…}` / `impl From<rc_index::IndexError> for ToolError`）。
//! Step 5 交付时它编译不过是设计好的红（TDD 的 RED 在编译期），Step 6 落地后恢复绿。
//!
//! 实测登记与控制器裁定 R46①：brief 在 `retired_表态不落抑制账_这条断言是阶段2的炸药` 的第四句
//! `f.attestation == Some(Stance::Retired)` 是**计划错**（永红，红在生产码的既有口径上：
//! `reader::judgement_cols` 的 `attestation` 子查询只认 `('corroborated','confirmed')`，
//! `crates/rc-memory/src/reader.rs:59-63`），已按裁定改写为 `== None`（见该用例内注释）。
//! 其余各句逐字未动。R47 另给本文件补了一条 live 侧 `source` 读数用例。
//! 修复轮 1（工单 R50）把 `两个来源的分型映射不许漂` 的比较点从 `(kind, code)` 两元组升成
//! `(kind, code, hint)` 三元，并把 `From<IndexError>` 那张表的八臂**逐臂**对撞进来；
//! 本文件其余用例逐字未动。

use rc_agent::recall::{recall, RecallFilter, RecallRequest, Viewer};
use rc_agent::tools::{
    remember, source_of, status, vote, RememberRequest, StatusOptions, VoteStance,
    MAX_REMEMBER_CHARS, MAX_STATUS_ENTITIES,
};
use rc_agent::{AgentErrorKind, CycleHealth, CycleRead, ToolError, TurnExtraction, TurnSink};
use rc_memory::{attest, put_card, put_entity, retire, set_verdict, AnchorRole,
                EntityContent, FactContent, Provenance, Scope, ScopeKind, Stance, Verdict};
use rc_search::GateCell;
use rc_store::{AgentRoot, Root};
use serde_json::Value;

const NOW: i64 = 1_700_000_000_000;
/// 只在本文件出现的金丝雀正文：`status` 与 `vote` 的返回里不许有它（守卫 12 的两个新面）。
const CANARY: &str = "金絲雀写入正文-4a71";

/// 假缝：只记参数。真实落库在任务 10 的 `mcp/tests/tools.rs` 上断（缝的两侧各测一半，见开头 (1)）。
#[derive(Default)]
struct Tap {
    calls: std::sync::Mutex<Vec<(String, String, i64)>>,
    /// 假缝的编排位。`None` 是**默认**，含义是"缝说这一轮不在转录里"（P-62 那条用它）；
    /// 既有那两条 `remember` 用例改走 `Tap::fresh()`。
    ///
    /// 这里出现 `"pending"` / `"rejected"` 这样的字面量**是有意的**：本 crate 的依赖清单里
    /// 没有 `rc-ledger`（那正是缝存在的理由），所以桩只能喂字符串。**§2.20 禁令 1 的 grep
    /// 管 `src` 不管 `tests`**，把它"修"成枚举就是给 `rc-agent` 加一条 `rc-ledger` 依赖 = 破守卫 6。
    extraction: std::sync::Mutex<Option<TurnExtraction>>,
    /// P5 的编排位。默认 `None` = **这条用例没打算测 P5**，缝在 `cycle_health` 上回一条
    /// `unavailable`——而不是回一个全零的 `CycleHealth`。理由与 P-54 同族：
    /// 一个从没被编排过的假缝回"0 条落后、0 次调用"，就是让每一存量 P2 用例都顺带测了一遍 P5。
    health: std::sync::Mutex<Option<CycleHealth>>,
    /// A1 的两格编排位，与 P5 的 `health` 同一形状：`#[derive(Default)]` 让 `i64` 落 `0`、
    /// `Vec` 落空，所以本文件既有用例**一个字都不用改**就已经编排了一个合法读数。
    /// 这正是本任务不许把它们挂到 `CycleHealth` 上的原因——挂上去就得逐个构造第六、七格，
    /// 而"挂上去"还顺手把两格拖进了"只在纯 status 出场"的那个坑。
    halted: i64,
    lag_scopes: Vec<(String, i64)>,
    /// P0-1 的编排位：有效章会话数。默认 `None` ⇒ 走 trait 的默认实现（`status` 渲染 `null`），
    /// 那正是"这道缝没接转录库"的真实形状。
    principal_claims: Option<i64>,
}
impl Tap {
    /// "刚写下去"那一格：水位库里还没有这条轮的状态行（§2.22 的默认读数来源），队列位置零。
    /// P5 任务 13 加了 `health` 字段：默认 `Tap::default()` 那条 `None` 路径**仍**走 `cycle_health`
    /// 返回 `unavailable`，所以"想跑过 P5"必须显式 `with_health`；`Tap::fresh()` 是给那些**只想
    /// 走 remember 后备**的用例的编排位——它默认**编排一个合法 P5 读数**（`lag=0、used=0`），
    /// 让既有 `status(default, …)` 的几条 P1/P2 用例不破。
    /// 单轮上限那一格已随 2026-11-06 的裁定删除，编排位因此少一格。
    fn fresh() -> Tap {
        Tap {
            extraction: std::sync::Mutex::new(Some(TurnExtraction {
                extract_state: "pending".into(), reason: None, eta_turns: 0,
            })),
            health: std::sync::Mutex::new(Some(rc_agent::CycleHealth {
                lag_turns: 0, last_success_at_ms: None,
                llm_calls_used: 0,
                last_derive_at_ms: None, derive_cursor: None,
                last_anomaly_scan_at_ms: None,
                quota_stopped: Some(false), // S3a：假缝默认编排"跑过一轮且没触顶"
                llm_calls_total: Some(0),
            })),
            ..Default::default()
        }
    }
    fn with_extraction(state: &str, reason: Option<&str>, eta: u64) -> Tap {
        Tap {
            extraction: std::sync::Mutex::new(Some(TurnExtraction {
                extract_state: state.into(), reason: reason.map(String::from), eta_turns: eta,
            })),
            ..Default::default()
        }
    }
    fn with_health(mut self, h: Option<CycleHealth>) -> Self { self.health = std::sync::Mutex::new(h); self }
}

// 新读数那两条用例的夹具：一个**空记忆库**的记忆柄。这两格与有没有记忆行无关，
// 所以这里不造行——与 `status_只给计数_一个正文都不带` 那条同源，只是把那三行收成一句。
//
// `TempDir` 与 `AgentRoot` 一起 `Box::leak`——两者必须同生共死：目录先析构的话，
// `TempDir` 在 Windows 上删不掉还开着的 `memory.db`，在 POSIX 上则把文件 unlink 成
// "句柄还活着但路径没了"的样子。放 `thread_local!` 而不是 `OnceLock`：rusqlite 的
// `Connection` 不是 `Sync`，而 `static` 要求 `Sync`（`OnceLock` 那条路在本文件实测过，编译期就红）。
thread_local! {
    static FRESH_MEMORY: &'static rc_store::MemoryHandle = {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let ar = Box::leak(Box::new(rc_store::AgentRoot::open(dir.path()).unwrap()));
        ar.memory()
    };
}

/// 空记忆库的记忆柄。`'static` 的来历写在上面那个 `thread_local!` 的注释里——
/// 简版：`Root` / `AgentRoot` 借出句柄，而用例要在夹具之外拿着句柄调 `status`。
fn fresh_memory() -> &'static rc_store::MemoryHandle {
    FRESH_MEMORY.with(|m| *m)
}
impl TurnSink for Tap {
    fn append_agent_turn(&self, session_key: &str, body: &str, now_ms: i64) -> Result<String, ToolError> {
        let mut c = self.calls.lock().unwrap();
        c.push((session_key.to_string(), body.to_string(), now_ms));
        // turn_uid 的形状与阶段 0 一致：64 位小写 hex
        Ok(format!("{:04x}{}", c.len(), "0".repeat(60)))
    }
}
impl CycleRead for Tap {
    fn turn_extraction(&self, _turn_uid: &str) -> Result<Option<TurnExtraction>, ToolError> {
        Ok(self.extraction.lock().unwrap().clone())
    }
    /// 没编排时给的是 `store_unreachable` 那一条**既有家**（`ToolError::unavailable`），不是新造一个码：
    /// P5 的缝失败在分型族里就是"存储读不到"，给它第二个码等于把同一个症状分成两个说法。
    /// 走 `unavailable` 而不是 `ToolError::from(StoreError)`：`impl From<…> for ToolError` 今天只有
    /// 三条——`SearchError` / `MemError` / `IndexError`，里面没有 `StoreError`——而 `rc-agent` 的
    /// error.rs 也不该为一条测试夹具多开一条"把存储的错误原样转述出去"的路（那与任务 10 的
    /// `store_down` 是同一个家）。
    fn cycle_health(&self) -> Result<CycleHealth, ToolError> {
        // `.clone()`：`CycleHealth` 去掉 `Copy` 之后从 `Mutex` 的借用里不许再搬出去。
        self.health.lock().unwrap().clone().map(Ok).unwrap_or_else(|| Err(rc_agent::ToolError::unavailable(
            "store_unreachable", "run: doctor")))
    }
    // 追加到既有 `impl CycleRead for Tap { … }` 块的末尾。**不要再写第二个
    // `impl CycleRead for Tap`**——同一类型同一 trait 的第二个 impl 是 E0119，
    // 整个测试文件会编不过，"红测试"也就没了。
    fn halted_scopes(&self) -> Result<i64, ToolError> { Ok(self.halted) }
    fn lag_scopes(&self) -> Result<Vec<(String, i64)>, ToolError> {
        Ok(self.lag_scopes.clone())
    }
    fn principal_claims(&self) -> Result<Option<i64>, ToolError> { Ok(self.principal_claims) }
}

fn card(dir: &std::path::Path, title: &str, body: &str) -> rc_memory::keys::MemKey {
    let root = Root::open(dir).unwrap();
    let k = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: title.into(),
            body: body.into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
    k
}

/// 指定归属的那一版 `card()`。唯一的使用者是 `提示不按归属筛_因为那一刻归属还不存在` 这一条（坑 P-68）——
/// 它要的是"两条都在库里、归属不同"，而既有 `card()` 的归属是写死的 `proj-synthetic`。
/// 复制而非改 `card()`：那会把 12 处既有调用点的形状全拖下水。
fn card_in(dir: &std::path::Path, scope_key: &str, title: &str, body: &str) -> rc_memory::keys::MemKey {
    let root = Root::open(dir).unwrap();
    let k = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: title.into(),
            body: body.into(),
            scope: Scope { kind: ScopeKind::Project, key: scope_key.into() },
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
    k
}

/// 七条用例共用的查询串（纪律 2：这对词在 `tools_shape.rs` 的
/// `正面表态没锚被拒_负面表态免锚且当场改闸门` 与 `正面表态把形态推到_full_V5_的阶梯在写侧闭环` 两条里
/// 已经被证明能召回 `card()` 那张行）。
const P4_QUERY: &str = "合并 测试";

/// 走一遍完整的 `remember`（三个实参都要真的：假缝 + 真记忆柄 + 真请求）。
/// 抽成函数是**为了让七条用例的差异只在夹具**，不在调用形状。
fn p4_out(dir: &std::path::Path, body: &str) -> Value {
    let root = Root::open(dir).unwrap();
    let tap = Tap::fresh();
    let req = RememberRequest::new(body).unwrap();
    // 相似提示的读者 = 夹具的署名 agent（`put_card` 走 `OwnerCtx::synthetic` ⇒
    // owner_agent_id = "synthetic-agent"）：这一族用例判的是 P4 自己的过滤，
    // 读闸那一维不能在这里掺变量。W37-A1（G16）：similar_hint 带这个 viewer 过读闸。
    v(remember(&tap, &tap, root.memory(),
        Some(&Viewer::agent("synthetic-agent")), &req, "sess-p4", NOW).unwrap().text())
}

fn v(s: String) -> Value {
    serde_json::from_str(&s).expect("返回不是合法 JSON")
}

// ── 任务 11（P3 实体目录）—— 三个夹具助手 ────────────────────────────────

/// 目录夹具的写口：实体 + 一张**共享 `turn` 的** card。`turn` 是 64 位 hex，
/// 与 `candidate_read.rs` 同一条理由——转录库在这层根本拿不到。
fn person_on(root: &Root, title: &str, aliases: &[&str], turn: &str, at: i64) -> rc_memory::keys::MemKey {
    rc_memory::put_entity(root.memory(), &rc_memory::EntityContent {
        kind: "person".into(), title: title.into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        aliases: aliases.iter().map(|s| s.to_string()).collect(),
    }, &[Provenance { turn_uid: turn.into(), anchor_role: AnchorRole::Human }], at).unwrap()
}

fn fact_on(root: &Root, title: &str, body: &str, turn: &str, at: i64) -> rc_memory::keys::MemKey {
    put_card(root.memory(), &FactContent {
        kind: "decision".into(), title: title.into(), body: body.into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: turn.into(), anchor_role: AnchorRole::Human }], at).unwrap()
}

/// 一次开库 + 建索引 + 返回目录格的便捷写法（`status` 的索引那一格在没建索引时是 `built:false`
/// 读数而不是错误，所以这里建不建都不影响目录；建了是为了不与本文件既有用例的夹具形状漂开）。
fn seeded(dir: &std::path::Path) -> Root {
    let root = Root::open(dir).unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
    root
}

fn cells(s: String) -> Vec<String> {
    // 收的是 `out["entities"][0].to_string()`，所以先 parse 回 `Value` 再看键。
    let mut v: Vec<String> = serde_json::from_str::<Value>(&s)
        .expect("status 的返回必须是合法 JSON")
        .as_object().expect("目录一格不是对象").keys().cloned().collect();
    v.sort();
    v
}

#[test]
fn remember_写一条转录_不建记忆行_并如实回报抽取态() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::fresh();
    let req = RememberRequest::new("先跑测试再合并，红的不许合。").unwrap();
    let out = v(remember(&tap, &tap, root.memory(), None, &req, "sess-1", NOW).unwrap().text());
    assert_eq!(tap.calls.lock().unwrap().len(), 1, "remember 只许经缝写一次");
    assert_eq!(out["schema_version"], 2);
    assert_eq!(out["extract_state"], "pending", "缝说'水位库里还没有这一轮'⇒ 默认读数；**不再**取自 capabilities（P-26/P-45）");
    assert_eq!(out["accepted_chars"], 14, "按字符数不是字节数（守卫 8 那族）");
    assert_eq!(out["turn_uid"].as_str().unwrap().chars().count(), 64);
    // 反向的一半——本阶段最容易被"顺手做掉"的一步：记忆库里一行都没多。
    assert!(
        rc_memory::reader::facts(root.memory()).unwrap().is_empty(),
        "GOALS §3 明确拒掉同步抽取：remember 之后库里出现新行就是越界"
    );
}

#[test]
fn remember_的上限按字符_超限是拒收不是截断() {
    let tap = Tap::default();
    let ok = "记".repeat(MAX_REMEMBER_CHARS);
    assert!(RememberRequest::new(&ok).is_ok(), "中文正文被按字节拒了 ⇒ 上限的含义变了");
    let e = RememberRequest::new(&"记".repeat(MAX_REMEMBER_CHARS + 1)).unwrap_err();
    assert_eq!(e.code, "remember_too_long");
    assert_eq!(e.kind, AgentErrorKind::Validation);
    assert!(
        !e.hint.chars().any(|c| c.is_ascii_digit()),
        "hint 里写死了数字 ⇒ 与常量两处真相，上限改一次要改两处（{}）",
        e.hint
    );
    assert!(tap.calls.lock().unwrap().is_empty(), "拒收的那一条一次都没写");
    // 无输入不可构造（守卫 5 的写入面形状）
    assert!(RememberRequest::new("").is_err());
    let blank = RememberRequest::new("   \n  \t ").unwrap_err();
    assert_eq!(blank.code, "empty_remember");
}

#[test]
fn 正面表态没锚被拒_负面表态免锚且当场改闸门() {
    let dir = tempfile::tempdir().unwrap();
    let k = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试，红的不许合。");
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let id = k.key_string();
    // ① 没锚的正面表态 ⇒ Validation（分型经 `From<MemError>::AnchorNotHuman` 那一个家）
    let e = vote(m, &id, VoteStance::Confirmed, None, None, NOW).unwrap_err();
    assert_eq!(e.code, "anchor_not_human", "{e:?}");
    // ② 负面表态免锚，成功
    let out = v(vote(m, &id, VoteStance::Rejected, None, Some("与另一条重复"), NOW).unwrap().text());
    assert_eq!(out["suppresses"], true);
    // ③ 写进去的东西**当场改变了闸门**——"表态 → 判定"这条闭环在本阶段只在这里闭合一次
    let view = rc_agent::surface::probe_via_search(m, None, "合并 测试", NOW).unwrap();
    let rejected_n = view
        .silent
        .counts()
        .into_iter()
        .find(|(c, _)| *c == GateCell::Rejected)
        .map(|(_, n)| n)
        .unwrap_or(0);
    assert!(rejected_n >= 1, "被否的行没落进 rejected 格：{:?}", view.silent.counts());
    assert!(view.forms.is_empty(), "被否的行还在被提起");
    // ④ 金丝雀：vote 的返回面零正文
    assert!(!out.to_string().contains("任何合并之前"), "vote 回显了正文（守卫 12）");
}

#[test]
// 用例名逐字取 brief 的"fn 正面表态把形态推到_full_V5_的阶梯在写侧闭环"那条（Step 4 的 export_doc 段）的大写 `V5` 形式，与 `export_doc.rs:25` 同判据：
// 这个名字是验收契约的一部分（任务 11 的 `one --exact` 点它的名），
// 所以这里放行的是 rustc 的 `non_snake_case` 风格 lint，不是命名纪律。
#[allow(non_snake_case)]
fn 正面表态把形态推到_full_V5_的阶梯在写侧闭环() {
    let dir = tempfile::tempdir().unwrap();
    let k = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。红的不许合。");
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 没有表态时：过闸门但只指路（signpost）
    let before = rc_agent::surface::probe_via_search(m, None, "合并 测试", NOW).unwrap();
    assert_eq!(before.forms.iter().map(|d| d.form).collect::<Vec<_>>(), vec![rc_search::Form::Signpost]);
    // `card()` 播的出处是人工轮，所以这个锚过 `attest` 的出处核对（V6：检索路径不开会话库）
    let out = v(vote(m, &k.key_string(), VoteStance::Corroborated, Some(&"h".repeat(64)), None, NOW)
        .unwrap()
        .text());
    assert_eq!(out["row_now"]["form"], "full", "{out}");
    let after = rc_agent::surface::probe_via_search(m, None, "合并 测试", NOW).unwrap();
    assert_eq!(after.forms.iter().map(|d| d.form).collect::<Vec<_>>(), vec![rc_search::Form::Full]);
    assert_eq!(after.forms[0].lines(), 2);
}

#[test]
fn retired_表态不落抑制账_这条断言是阶段2的炸药() {
    let dir = tempfile::tempdir().unwrap();
    let k = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let out = v(vote(m, &k.key_string(), VoteStance::Retired, None, Some("判为无价值"), NOW)
        .unwrap()
        .text());
    assert_eq!(out["suppresses"], false, "agent 面构造不出删除（架构 §3）");
    assert_eq!(out["row_now"]["silence_reason"], Value::Null, "Retired 不该让这一行沉默");
    let f = rc_memory::reader::lookup(m, &k).unwrap().unwrap();
    assert!(!f.retired, "阶段 1 的 agent 面不许写抑制账（那是 C 的 retire）");
    // 裁定 R46①（改写 brief 的第四句）：`reader::judgement_cols` 的 attestation 子查询只认
    // ('corroborated','confirmed')（`crates/rc-memory/src/reader.rs:59-63`），Retired 表态在这张
    // 正面读数面上读回来就是 None —— brief 的 `Some(Stance::Retired)` 永红（红在计划自己的口径上）。
    // 这一句现在是"Retired 不进正面表态读数面"的**读数**：把那个 IN 子句加上 'retired' 它就红。
    assert_eq!(f.attestation, None, "Retired 不进正面表态的读数面");
    // 上两句（`suppresses == false`、`!f.retired`）才是本条用例的判据；阶段 2 若把 `Retired`
    // 接到抑制账，`!f.retired` 必须红一次并被改写——那就是它的牙。
}

#[test]
fn status_只给计数_一个正文都不带() {
    let dir = tempfile::tempdir().unwrap();
    // 编译修正（brief 原文给 `card` 传的是 `format!` 的 String 而签名收 `&str`，E0308）：加 `&`，值不变。
    let _k = card(dir.path(), "先跑测试再合并", &format!("{CANARY} 合并前必测。"));
    let root = Root::open(dir.path()).unwrap();
    let out = v(status(root.memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    let s = out.to_string();
    assert!(!s.contains(CANARY) && !s.contains("合并前必测"), "status 带了正文 ⇒ 诊断面破约（守卫 12）");
    assert_eq!(out["source"], "live");
    assert_eq!(out["rows"]["card"], 1);
    assert_eq!(out["rows"]["anchor"], 0);
    assert_eq!(out["open_rejections"], 0);
    assert_eq!(out["dangling_attestations"], 0);
    assert_eq!(out["extract_state"], "queryable", "能力声明：状态**可反查**，不是'还没抽'（P-26 的 :192 那一处，值要动）");
    assert_eq!(out["index"]["built"], true);
    assert_eq!(out["index"]["rows_indexed"], 1);
    assert_eq!(out["capabilities"]["max_remember_chars"], MAX_REMEMBER_CHARS as u64);
    // 空库上是全零读数，不是错误："库里没有东西"与"读不到库"必须分得开（分型族那条）
    let empty = tempfile::tempdir().unwrap();
    let r = Root::open(empty.path()).unwrap();
    let z = v(status(r.memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!(z["rows"]["card"], 0);
    assert_eq!(z["index"]["built"], false, "没建索引在 status 面上是读数不是错误");
    assert_eq!(z["source"], "live");
}

#[test]
fn 副本面响亮标_replica_且写入一律拒() {
    let src = tempfile::tempdir().unwrap();
    let (snap, id) = {
        let root = Root::open(src.path()).unwrap();
        let k = rc_memory::synth::seed(root.memory(), NOW).unwrap().remove(0);
        rc_index::rebuild(root.memory(), NOW).unwrap();
        root.memory()
            .conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        let p = src.path().join("snap.db");
        std::fs::copy(src.path().join("memory.db"), &p).unwrap();
        (p, k.key_string())
    };
    let home = tempfile::tempdir().unwrap();
    let ar = AgentRoot::open_replica(home.path(), &snap).unwrap();
    let m = ar.memory();
    assert_eq!(source_of(m), "replica");
    // 三个只读面都带 source（架构 §6 要求副本"响亮标 source: replica"）
    assert_eq!(v(status(m, None, &StatusOptions::default(), NOW).unwrap().text())["source"], "replica");
    let r = v(recall(m, &RecallRequest::query("同分合成词甲").unwrap(), &RecallFilter::empty(), NOW).unwrap().text());
    assert_eq!(r["source"], "replica");
    let doc = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap()
        .text();
    assert!(doc.contains("source: replica"), "导出件没标副本 ⇒ 人会拿一周前的快照当现状：{doc}");
    // 写入面一律响亮拒绝（架构 §6："绝不在本地排队等上行"）
    let e = vote(m, &id, VoteStance::Rejected, None, Some("格子"), NOW).unwrap_err();
    assert_eq!(e.kind, AgentErrorKind::Authorization, "{e:?}");
    assert_eq!(e.code, "replica_read_only");
}

/// 一张分型表里**一条臂**的三面对撞（工单 R50 第 1 件）：同一条 `IndexError` 变体经
/// `From<IndexError>` 与经 `From<SearchError>` 的 `SE::Index(..)` 各走一遍，比 `kind`、`code`、
/// `hint` 三个维度。R40 把"委托"否掉、保留第二份表的唯一理由就是这条用例必须有牙
/// （`task-8-report.md` 第二段 §1），而只比两元组的声明是假牙：hint 维度当时一个读者都没有。
/// `#[track_caller]` 让红点落在**调用它的那一行**（也就是漂的那条臂），不落在 helper 内部。
/// 断言消息只带臂名（slug）与 `{direct:?}`/`{via_search:?}`：`ToolError` 的 Debug 是
/// kind/码/hint 三个常量串，不含载荷 ⇒ 守卫 12 的零正文/零路径不破。
#[track_caller]
fn 三面对撞(arm: &str, direct: ToolError, via_search: ToolError) {
    assert_eq!(
        (direct.kind, direct.code, direct.hint),
        (via_search.kind, via_search.code, via_search.hint),
        "{arm} 臂的两条路漂了：{direct:?} vs {via_search:?}"
    );
}

#[test]
fn 两个来源的分型映射不许漂() {
    // 同一件事（索引没建）从 `IndexError` 与 `SearchError::Index(..)` 两条路进来，
    // 必须落进同一个格、同一个码。两处各写一遍映射就是下一次漂移的地方。
    use rc_index::IndexError as IE;
    use rc_search::SearchError as SE;
    let a: ToolError = rc_index::IndexError::NotBuilt.into();
    let b: ToolError = rc_search::SearchError::Index(rc_index::IndexError::NotBuilt).into();
    assert_eq!((a.kind, a.code, a.hint), (b.kind, b.code, b.hint), "{} vs {}", a.code, b.code);
    let c: ToolError = rc_index::IndexError::NoQueryTokens.into();
    let e: ToolError = rc_search::SearchError::Index(rc_index::IndexError::NoQueryTokens).into();
    assert_eq!((c.kind, c.code, c.hint), (e.kind, e.code, e.hint));
    // 裁定 R45（第三段）：`SE::Store(StoreError::Io|Sql)` 两支的**运行时读数**此前无人读
    // （拆 wildcard 只兑现了 E0004 那条腿；⇒ store_unreachable 这条真值要有活体读者）。
    // 同时把这两支对撞进两张表的接缝：索引层与存储层两条路进来的 store 故障必须同格同码。
    let g: ToolError = rc_search::SearchError::Store(rc_store::StoreError::Io("io".to_string())).into();
    assert_eq!((g.kind, g.code, g.hint), (AgentErrorKind::Unavailable, "store_unreachable", "run: doctor"), "{g:?}");
    let h: ToolError = rc_search::SearchError::Store(rc_store::StoreError::Sql("sql".to_string())).into();
    assert_eq!((h.kind, h.code, h.hint), (AgentErrorKind::Unavailable, "store_unreachable", "run: doctor"), "{h:?}");
    let i: ToolError = rc_index::IndexError::Store(rc_store::StoreError::Io("io".to_string())).into();
    assert_eq!((i.kind, i.code, i.hint), (g.kind, g.code, g.hint), "{} vs {}", i.code, g.code);
    // T3a：canonical 之前的旧库**没有迁移**，`rc-store` 开门即报 `SchemaRebuildRequired`。
    // 这一支同样是逐变体点名表里的一行，所以给它一个活体读者：分型是 Unavailable、
    // 码归既有的 `store_unreachable` 一族、hint 固定 `run: doctor`（操作员的下一步由 doctor 点名）。
    // 拿掉 `From<SearchError>` 里那一行，本句就是 E0004；把它改并进 `schema_too_new`，本句红在码上。
    let r: ToolError = rc_search::SearchError::Store(rc_store::StoreError::SchemaRebuildRequired {
        db: "memory.db", found: 5, supported: 6,
    }).into();
    assert_eq!((r.kind, r.code, r.hint), (AgentErrorKind::Unavailable, "store_unreachable", "run: doctor"), "{r:?}");
    // ── 工单 R50（Important #1）：上面那几条原本只比 `(kind, code)` 两元组 ⇒ `src/error.rs` 的
    //    `From<IndexError>` 那张表里 NormMismatch / LayerUnreadable / Sql / Memory /
    //    ReadOnly+Store(ReadOnly) 合并臂 **五臂的码写漂全仓绿**，而八臂的 hint 一个读者都没有。
    //    下面把八臂逐臂对撞（`NotBuilt`/`NoQueryTokens` 在上面，`Store(_)` 兜底支在下面这支补齐），
    //    三个维度：kind、码、hint。两条路的载荷各构造各的，形状相同；
    //    `Sql`/`Store` 两支用本测试自己的 `format!` 串（载荷不进 hint，也不碰 `CANARY`）。
    三面对撞("NotBuilt", IE::NotBuilt.into(), SE::Index(IE::NotBuilt).into());
    三面对撞("NoQueryTokens", IE::NoQueryTokens.into(), SE::Index(IE::NoQueryTokens).into());
    三面对撞(
        "NormMismatch",
        IE::NormMismatch { marker: 1, current: 2 }.into(),
        SE::Index(IE::NormMismatch { marker: 1, current: 2 }).into(),
    );
    三面对撞(
        "LayerUnreadable",
        IE::LayerUnreadable.into(),
        SE::Index(IE::LayerUnreadable).into(),
    );
    三面对撞(
        "Sql",
        IE::Sql(format!("synthetic-{}", 1)).into(),
        SE::Index(IE::Sql(format!("synthetic-{}", 1))).into(),
    );
    三面对撞(
        "Memory",
        IE::Memory(rc_memory::MemError::ReadOnly).into(),
        SE::Index(IE::Memory(rc_memory::MemError::ReadOnly)).into(),
    );
    // `ReadOnly` 与 `Store(ReadOnly)` 在 `From<IndexError>` 那张表里是**一条合并臂**（配或，
    // `IE::ReadOnly(_) | IE::Store(StoreError::ReadOnly(_))` 那一支），在 `From<SearchError>`
    // 那张表里是**两支行**（`SE::Index(IndexError::ReadOnly(_))` 与
    // `SE::Index(IndexError::Store(StoreError::ReadOnly(_)))`）
    // ⇒ 这里是 1↔2 的三个读数，不是有一支对撞不上。
    // 真被"收窄"掉的是兜底支里的 ReadOnly 载荷：两张表里它都先被上面那支接走
    // （`IE::Store(_)` 兜底支排在那条或臂之后、`SE::Index(IndexError::Store(_))` 排在那两支行之后），
    // 所以兜底支这一支的对撞**必须**用非 ReadOnly 的载荷，用 ReadOnly 就是在测上面那支。
    三面对撞("ReadOnly", IE::ReadOnly("synthetic").into(), SE::Index(IE::ReadOnly("synthetic")).into());
    三面对撞(
        "Store(ReadOnly)",
        IE::Store(rc_store::StoreError::ReadOnly("synthetic")).into(),
        SE::Index(IE::Store(rc_store::StoreError::ReadOnly("synthetic"))).into(),
    );
    三面对撞(
        "Store(兜底)",
        IE::Store(rc_store::StoreError::Io(format!("synthetic-{}", 2))).into(),
        SE::Index(IE::Store(rc_store::StoreError::Io(format!("synthetic-{}", 2)))).into(),
    );
}

#[test]
fn source_在_live_面上同样是读数() {
    // 裁定 R47：`source` 这个键在 Step 5 的用例里只有**副本侧**读者（`副本面响亮标_replica…`）
    // 与 status 的 live 读者。⇒ `recall` 面或 `export` 面把 source 写死成常量 `"replica"`
    // 全仓不红（Step 9 第 7 项只验"删掉那一行"，验不了"写死"）。一条从未红过的守卫等于没有守卫，
    // 所以这里给两个 live 面各补一条读数：写死 replica 会在下面两句上红。
    let dir = tempfile::tempdir().unwrap();
    let _k = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let r = v(recall(m, &RecallRequest::query("合并 测试").unwrap(), &RecallFilter::empty(), NOW).unwrap().text());
    assert_eq!(r["source"], "live", "recall 面读不出 live ⇒ source 是常量不是身份读数");
    let doc = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap()
        .text();
    assert!(doc.contains("source: live"), "导出件没标 live ⇒ 身份行是写死的：{doc}");
}

#[test]
fn vote_stance_的_slug_双向唯一() {
    // `from_slug` 住在 tools.rs 的理由：四个 slug 字符串只能有一个家（守卫 3），
    // 而任务 10 的 wire 面只有字符串可读。
    for s in VoteStance::ALL {
        assert_eq!(VoteStance::from_slug(s.slug()), Some(s), "{:?} 的逆不成立", s);
    }
    // 精确匹配：不 trim、不折大小写。`"Confirmed"` 是宿主的 bug，不是我们的规范化职责；
    // 宽容一点的结果是 `bad_stance` 那一型永远测不到（任务 10 Step 5 有它的用例）。
    for bad in ["", "Confirmed", "confirmed ", "delete", "unknown", "CONFIRMED"] {
        assert_eq!(VoteStance::from_slug(bad), None, "{bad} 不该被认成表态");
    }
}

// ── 任务 10（P2 读缝）：九条新用例 ────────────────────────────────────────────

#[test]
fn remember_的读数不来自_capabilities_声明() {
    // P-45①。`Tap::default()`（缝说"查无"）在这里会红成 `turn_unreadable`，
    // 所以这条同时是"读数不是从声明里抄的"的证据：声明恒 `queryable`，读数由缝给。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::with_extraction("extracted", None, 3);
    let req = RememberRequest::new("先跑测试再合并，红的不许合。").unwrap();
    let out = v(remember(&tap, &tap, root.memory(), None, &req, "sess-1", NOW).unwrap().text());
    assert_eq!(out["extract_state"], "extracted", "缝给的是 extracted，面上却报了别的 ⇒ 有人在翻译");
    assert_ne!(out["extract_state"], rc_agent::capabilities().extract_state,
               "读数等于声明 ⇒ 同源，四态在写入面上永远读不出（P-45）");
    assert_eq!(out["eta_turns"], 3);
    assert_eq!(out["reason"], Value::Null, "无理由时那一格是 null 不是空串");
}

#[test]
fn 四态各自原样穿过缝_一个都不许被翻译() {
    // §10 补口①"pending 与 rejected 必须在同一个查询面上可区分"在单元侧的落点。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    for state in ["pending", "extracted", "rejected", "worthless"] {
        let tap = Tap::with_extraction(state, Some("合成理由"), 7);
        let req = RememberRequest::new("一条合成正文").unwrap();
        let out = v(remember(&tap, &tap, root.memory(), None, &req, "s", NOW).unwrap().text());
        assert_eq!(out["extract_state"], state, "{state} 被改了说法 ⇒ 面上有一张翻译表");
        assert_eq!(out["reason"], "合成理由", "reason 是透传，不是重新措辞（P-46②）");
        assert_eq!(out["eta_turns"], 7);
    }
}

#[test]
fn 缝的载荷是三个字段_一条正文都不带() {
    // P-46 的两条牙：① 数格数（与 P-16 那把八格尺子同型）；② 正文 canary 不许出现在回执里。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::with_extraction("rejected", Some("与另一条重复"), 2);
    let req = RememberRequest::new(CANARY).unwrap();
    let out = v(remember(&tap, &tap, root.memory(), None, &req, "s", NOW).unwrap().text());
    let t = serde_json::to_value(&TurnExtraction {
        extract_state: "rejected".into(), reason: Some("与另一条重复".into()), eta_turns: 2,
    }).unwrap();
    assert_eq!(t.as_object().unwrap().len(), 3, "缝多带了一个字段（守卫 12 从读缝这侧破）");
    assert!(out["extract_state"].is_string() && out["eta_turns"].is_number());
    // `remember` 的顶层**含**用户自己刚写的那句话的**长度**，但不含那句话本身；
    // 而 `TurnExtraction` 三格里若混进正文，`fields()` 就会把它摊平到顶层。
    assert!(!out.to_string().contains(CANARY), "回执里出现了正文（守卫 12）");
}

#[test]
fn status_带_turn_时把三格挂在_extraction_下面() {
    let dir = tempfile::tempdir().unwrap();
    let _k = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::with_extraction("extracted", None, 11);
    let uid = "9".repeat(64);
    let opts = StatusOptions { turn: Some(&uid), ..Default::default() };
    let out = v(status(root.memory(), Some(&tap), &opts, NOW).unwrap().text());
    assert_eq!(out["extraction"]["extract_state"], "extracted");
    assert_eq!(out["extraction"]["eta_turns"], 11);
    assert_eq!(out["extract_state"], "queryable",
               "顶层那一格是**能力声明**，与 `extraction` 里的**每轮读数**同在一张 JSON 里且不相等（P-26）");
    assert_ne!(out["extraction"]["extract_state"], out["extract_state"]);
}

#[test]
fn a_ghost_handle_on_status_is_empty() {
    // P-62 的第一半：形状对、库里没有 ⇒ Empty（与 `vote` 的 `row_absent` 同族）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::default(); // 缝报"查无此轮"
    let uid = "f".repeat(64);
    let e = status(root.memory(), Some(&tap), &StatusOptions { turn: Some(&uid), ..Default::default() }, NOW).unwrap_err();
    assert_eq!((e.kind, e.code), (AgentErrorKind::Empty, "turn_absent"), "{e:?}");
}

#[test]
fn remember_的缝读不到刚写的轮是_unavailable() {
    // P-62 的第二半：**同一个 `None`，另一个分型**（这条刚写完自己那句话，读不到就是库不一致）。
    // 这两条用例合起来才是"缝递数据不递判定"的运行期证据（§2.20 禁令 2 的另一条证据是 grep）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::default();
    let req = RememberRequest::new("一条合成正文").unwrap();
    let e = remember(&tap, &tap, root.memory(), None, &req, "s", NOW).unwrap_err();
    assert_eq!((e.kind, e.code), (AgentErrorKind::Unavailable, "turn_unreadable"), "{e:?}");
    assert_eq!(tap.calls.lock().unwrap().len(), 1, "转录**已经写了**：这一条 Err 不许把写也撤掉（P-61 同族）");
}

#[test]
fn 节点面上问抽取态是响亮拒绝() {
    // `read == None` 且 `turn` 给了 ⇒ Unavailable `extraction_not_readable`。
    // **不是给 null**：这一格是调用方主动要的，null 会把"这一面答不了"读成"这一轮没有状态"。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let uid = "a".repeat(64);
    let e = status(root.memory(), None, &StatusOptions { turn: Some(&uid), ..Default::default() }, NOW).unwrap_err();
    assert_eq!((e.kind, e.code), (AgentErrorKind::Unavailable, "extraction_not_readable"), "{e:?}");
    // 而**没问**就一切照旧：hook 的 brief 面（`read = None`、`turn = None`）不因此多一种失败。
    assert!(status(root.memory(), None, &StatusOptions::default(), NOW).is_ok());
}

#[test]
fn 形状错的轮句柄是_validation_不是查无() {
    // 与 `vote` 的 `bad_handle` / `row_absent` 同族：**先分形状、再问存在**。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::fresh();
    let bad: Vec<String> = vec![String::new(), "zz".repeat(32), "F".repeat(64), "a".repeat(63)];
    for b in &bad {
        let e = status(root.memory(), Some(&tap), &StatusOptions { turn: Some(b.as_str()), ..Default::default() }, NOW).unwrap_err();
        assert_eq!((e.kind, e.code), (AgentErrorKind::Validation, "bad_turn_handle"), "{b} ⇒ {e:?}");
    }
}

#[test]
fn 阶段1的格子一条都不许因为新增而消失() {
    // §10 补口②（"未知字段忽略、已知字段不许撤"）在本仓**唯一有牙的一半**：生产者侧。
    // 宿主那一半没有可读的代码，任务 13 把它登记进 A 表，不在这里假装测过。
    let dir = tempfile::tempdir().unwrap();
    let _k = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::fresh();
    let uid = "b".repeat(64);
    let with = v(status(root.memory(), Some(&tap), &StatusOptions { turn: Some(&uid), ..Default::default() }, NOW).unwrap().text());
    let without = v(status(root.memory(), Some(&tap), &StatusOptions::default(), NOW).unwrap().text());
    for k in ["schema_version", "source", "rows", "open_rejections", "open_retirements",
              "expired", "cooling_down", "dangling_attestations", "index", "extract_state", "capabilities"] {
        assert!(with.get(k).is_some() && without.get(k).is_some(), "{k} 被新增挤掉了（C2 是加法）");
    }
    assert_eq!(with["schema_version"], 2, "M2.5 改默认视图 ⇒ 版本 2；加字段仍不动版本号");
    assert!(with.get("extraction").is_some() && without.get("extraction").is_none(),
            "没问就不问：那一格不是给 null 占位，而是根本不出现（占位会把'没问'与'查无'揉成一个）");
}

// ── 任务 11（P3 实体目录）—— 七条用例 ─────────────────────────────────────────────

#[test]
fn status_无参时长出目录三格_每格六项() {
    let dir = tempfile::tempdir().unwrap();
    let root = seeded(dir.path());
    let e = person_on(&root, "老王", &["老王", "某老板"], &"1".repeat(64), NOW);
    fact_on(&root, "甲", "老王定的口径", &"1".repeat(64), NOW);

    let out = v(status(root.memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    assert!(out["entities"].is_array());
    assert_eq!(out["entities"].as_array().unwrap().len(), 1);
    assert_eq!(out["entities_total"], 1);
    assert_eq!(out["entities_unlisted"], 0);
    // P-50：六格逐格数。`serde_json::Map` 没有 `preserve_order`（本机已核根 `Cargo.toml`），
    // 所以键名在**序列化时就是字典序**——这一串字面量是字典序而不是书写序，别"顺手"改成书写序。
    assert_eq!(cells(out["entities"][0].to_string()),
               ["aliases", "id", "kind", "latest_as_of_ms", "name", "refs"]);
    let one = &out["entities"][0];
    assert_eq!(one["id"], e.id);
    assert_eq!(one["kind"], "person");
    assert_eq!(one["name"], "老王");                       // 身份不是正文（P-50）
    assert_eq!(one["aliases"], serde_json::json!(["某老板", "老王"]));   // 字典序：BTreeMap 的键序在这里第二次显形
    assert_eq!(one["refs"], 1);
    assert_eq!(one["latest_as_of_ms"], NOW);
}

#[test]
fn 目录里一格正文都没有() {
    // P-50 的另一半：守卫 12 的 CANARY 尺子原样搬到目录上。红的必须是不合法的那一侧。
    let dir = tempfile::tempdir().unwrap();
    let root = seeded(dir.path());
    person_on(&root, "老王", &["老王"], &"1".repeat(64), NOW);
    fact_on(&root, "甲", &format!("{CANARY} 这条正文不许出现在目录里"), &"1".repeat(64), NOW);

    let s = status(root.memory(), None, &StatusOptions::default(), NOW).unwrap().text();
    assert!(!s.contains(CANARY), "目录带了正文 ⇒ 守卫 12 从 P3 这一侧破了");
    assert!(!s.contains("这条正文不许出现在目录里"), "同上，换个片段再查一次（防 CANARY 恰好被截掉）");
}

#[test]
fn entity_过滤命中标题也命中别名而两边都归一() {
    // P-51 的三条断句：别名入口、尾随空格、NFC/NFD 之外的空白形状。
    let dir = tempfile::tempdir().unwrap();
    let root = seeded(dir.path());
    let a = person_on(&root, "老王", &["老王", "某老板"], &"1".repeat(64), NOW);
    let b = person_on(&root, "老李", &["李总"], &"2".repeat(64), NOW);

    let by_alias = v(status(root.memory(), None,
        &StatusOptions { entity: Some("某老板"), ..Default::default() }, NOW).unwrap().text());
    assert_eq!(by_alias["entities_total"], 1, "{by_alias}");
    assert_eq!(by_alias["entities"][0]["id"], a.id);

    // 尾随空格：库里存的仍是入参原样（`writers_x.rs:97-101`），而 id 是归一后算的，
    // 所以 `WHERE title = ?` 那种等值会在这里读成"库里没这个人"。
    for probe in ["老王 ", " 老王", "老 王"] {
        let got = v(status(root.memory(), None,
            &StatusOptions { entity: Some(probe), ..Default::default() }, NOW).unwrap().text());
        if probe == "老 王" {
            assert_eq!(got["entities_total"], 0, "中间有空格是**另一个名字**，不许模糊命中");
        } else {
            assert_eq!(got["entities_total"], 1, "{probe:?} 该命中同一个人：{got}");
        }
    }
    let other = v(status(root.memory(), None,
        &StatusOptions { entity: Some("李总"), ..Default::default() }, NOW).unwrap().text());
    assert_eq!(other["entities"][0]["id"], b.id, "过滤不许永远命中第一条（夹具得有区分）");
}

#[test]
fn 查无此人时目录是空数组而不是错误() {
    let dir = tempfile::tempdir().unwrap();
    let root = seeded(dir.path());
    person_on(&root, "老王", &["老王"], &"1".repeat(64), NOW);
    fact_on(&root, "甲", "邻居", &"1".repeat(64), NOW);

    let out = v(status(root.memory(), None,
        &StatusOptions { entity: Some("查无此人"), ..Default::default() }, NOW).unwrap().text());
    assert!(out["entities"].is_array() && out["entities"].as_array().unwrap().is_empty());
    assert_eq!(out["entities_total"], 0);
    assert_eq!(out["entities_unlisted"], 0);
    // 其余读数一格都不许多——"目录查不到"不许把整个 status 吞掉。
    assert_eq!(out["rows"]["anchor"], 1);
    assert_eq!(out["rows"]["card"], 1);
    assert_eq!(out["source"], "live");
    assert!(out.get("extraction").is_none(), "无 turn 参数时不该长出 extraction 那一格");
}

#[test]
fn turn_与_entity_互斥而空实体名是_validation() {
    // §2.14 那条互斥的进程内半边（线上半边在 `mcp/tests/p3_directory.rs`）。
    let dir = tempfile::tempdir().unwrap();
    let root = seeded(dir.path());
    let ghost = "f".repeat(64);

    let e = status(root.memory(), None,
        &StatusOptions { turn: Some(&ghost), entity: Some("老王"), ..Default::default() }, NOW).unwrap_err();
    assert_eq!((e.kind, e.code), (AgentErrorKind::Validation, "turn_and_entity"), "{e:?}");

    for blank in ["", "   ", "\u{3000}"] {                 // 全角空白也过一次归一尺子
        let e = status(root.memory(), None,
            &StatusOptions { entity: Some(blank), ..Default::default() }, NOW).unwrap_err();
        assert_eq!((e.kind, e.code), (AgentErrorKind::Validation, "empty_entity"), "{blank:?}");
    }
    // 互斥的两半各自单独给仍然合法（这条防的是"守卫写成了 turn 在场就一律拒"）。
    assert!(status(root.memory(), None, &StatusOptions::default(), NOW).is_ok());
    assert!(status(root.memory(), None, &StatusOptions { entity: Some("老王"), ..Default::default() }, NOW).is_ok());
}

#[test]
fn 目录截断到上限并且明写还有几个未列出() {
    // P-52：截断 + `unlisted_count` 的第三个调用点。夹具取 23 = 上限 + 3，
    // 让"截断本身"与"未列出数"同时非平凡（20/23 与 0 都不够——0 会随夹具一起漂）。
    let dir = tempfile::tempdir().unwrap();
    let root = seeded(dir.path());
    let mut ids = Vec::new();
    for n in 0..(MAX_STATUS_ENTITIES + 3) {
        ids.push(person_on(&root, &format!("人{n:02}"), &[], &format!("{n:0>64x}"), 1_000 + n as i64).id);
    }
    let out = v(status(root.memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!(out["entities"].as_array().unwrap().len(), MAX_STATUS_ENTITIES);
    assert_eq!(out["entities_total"], (MAX_STATUS_ENTITIES + 3) as u64);
    assert_eq!(out["entities_unlisted"], 3);
    // 三格自洽（这一句是 P-52 的正断句：不许有人把 `unlisted` 自己减出来）。
    let listed = out["entities"].as_array().unwrap().len() as u64;
    assert_eq!(listed + out["entities_unlisted"].as_u64().unwrap(), out["entities_total"].as_u64().unwrap());
    // 截断必须**有序**，否则第二次调用的 20 条与第一次的 20 条可以是不同二十条。
    let got: Vec<&str> = out["entities"].as_array().unwrap().iter().map(|e| e["id"].as_str().unwrap()).collect();
    let mut sorted = got.clone();
    sorted.sort_unstable();
    assert_eq!(got, sorted, "目录没按 id 升序 ⇒ 截断窗口不确定");
}

#[test]
fn 被退场的实体离开目录而_rows_anchor_不动() {
    // P-66 的**面**半边（库半边在 `candidate_read`）：目录少一条而 `rows.anchor` 一格不动，
    // 这两个数本来就是两个问题。`entities_total` 不许有人为了"一致"去对齐其中一边。
    let dir = tempfile::tempdir().unwrap();
    let root = seeded(dir.path());
    let keep = person_on(&root, "在场", &["在场"], &"1".repeat(64), NOW);
    let gone = person_on(&root, "退场", &["退场"], &"2".repeat(64), NOW);
    rc_memory::retire(root.memory(), &gone, "确认不再是话题", NOW + 1).unwrap();

    let out = v(status(root.memory(), None, &StatusOptions::default(), NOW + 2).unwrap().text());
    assert_eq!(out["entities_total"], 1, "{out}");
    assert_eq!(out["entities"][0]["id"], keep.id);
    assert_eq!(out["rows"]["anchor"], 2, "全表行数口径不许因为目录而改（阶段 1 就是这个口径）");
}

// ── 任务 12（P4 写侧重复提示）—— 七条用例 ────────────────────────────────

#[test]
fn 相似提示只列过线的那几条() {
    // 坑 P-15 的正面 + 档位谓词唯一的家。两条都在 FTS 里命中，但长的第二条整除到 0 分。
    // 长度口径同 `rc-search/tests/gate_switches.rs` 那条 `开关_above_floor_…`：0 分要
    // `row_len > 1024 · matched`，查询 `"合并 测试"` 最多两个不同词 ⇒ 正文里那两个词各重复 6000 次
    // ⇒ row_len ≈ 18000 ≫ 2048。
    let dir = tempfile::tempdir().unwrap();
    let a = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let _long = card(dir.path(), "长文低分卡", &"合并 测试 填充 ".repeat(6000));
    let out = p4_out(dir.path(), P4_QUERY);
    assert_eq!(out["similar_n"], 1, "过线过滤没生效 ⇒ 0 分的长行也被算成重复：{out}");
    assert_eq!(out["similar"][0]["id"], a.key_string());
    assert_eq!(out["similar"][0]["title"], "先跑测试再合并");
}

#[test]
fn 提示不按归属筛_因为那一刻归属还不存在() {
    // 坑 P-68 的钉子：跨归属的相似行**也**进提示。这条用例断的是"它进"，与上面几条相反——
    // 它是范围决定的证据，不是漏筛的证据。判同（`rows_in_scope`）按归属筛，这一层不能：
    // `remember` 收到的是一条转录，归属要到抽取那一刻才存在。
    let dir = tempfile::tempdir().unwrap();
    let a = card_in(dir.path(), "proj-synthetic", "先跑测试再合并", "任何合并之前先跑全量测试。");
    let b = card_in(dir.path(), "proj-other", "测试红了不许合", "任何合并之前先跑全量测试。");
    let out = p4_out(dir.path(), P4_QUERY);
    assert_eq!(out["similar_n"], 2, "跨归属的那条被筛掉了 ⇒ 那条边界被当成了 bug 修掉：{out}");
    let ids: Vec<String> = out["similar"].as_array().unwrap().iter()
        .map(|x| x["id"].as_str().unwrap().to_string()).collect();
    assert!(ids.iter().any(|x| x == &a.key_string()) && ids.iter().any(|x| x == &b.key_string()), "{out}");
}

#[test]
fn 实体行绝不进提示_连计数都不算它() {
    // 坑 P-60。故意让实体的**标题**与查询高度重合：它必然 FTS 命中、必然过线（标题短），
    // 于是"筛掉实体"这一支没有第二条路可走。
    let dir = tempfile::tempdir().unwrap();
    let a = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let root = Root::open(dir.path()).unwrap();
    let ent = put_entity(root.memory(), &EntityContent {
        kind: "person".into(), title: "合并测试".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        aliases: vec![],
    }, &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }], NOW).unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();
    drop(root);

    let out = p4_out(dir.path(), P4_QUERY);
    assert_eq!(out["similar_n"], 1, "实体行进了计数：{out}");
    assert_eq!(out["similar"][0]["id"], a.key_string());
    assert!(!out.to_string().contains(&ent.key_string()), "提示指向一条实体行（{out}）");
}

#[test]
fn 已退场的行不进提示_那是一条说不出来的已有() {
    // 坑 P-67。单变量：两条同样过线的行，只是其中一条被退场。`resolve_retirement` 不在这里用
    // （`candidate_read.rs` 那条对撞已经证明筛掉的是**账**），这里只钉 agent 面的读数。
    let dir = tempfile::tempdir().unwrap();
    let a = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let b = card(dir.path(), "先跑测试再合并 乙", "任何合并之前先跑全量测试。");
    assert_eq!(p4_out(dir.path(), P4_QUERY)["similar_n"], 2, "夹具没让两条都命中");
    let root = Root::open(dir.path()).unwrap();
    rc_memory::retire(root.memory(), &b, "p4:vote", NOW).unwrap();
    let out = p4_out(dir.path(), P4_QUERY);
    assert_eq!(out["similar_n"], 1, "被退场的行还在计数里 ⇒ 提示说了一条正在离开系统的已有：{out}");
    assert_eq!(out["similar"][0]["id"], a.key_string());
}

#[test]
fn 从没建过索引的那一次是没比过_不是没有相似() {
    // 坑 P-59① + P-61。故意不走 `card()`（它内含 rebuild）：`Root::open` 之后直接写行。
    // 第二句断言是 P-61：**降级不许把一条成功的写变成失败**。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    put_card(root.memory(), &FactContent {
        kind: "decision".into(), title: "先跑测试再合并".into(),
        body: "任何合并之前先跑全量测试。".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }], NOW).unwrap();
    let tap = Tap::fresh();
    let req = RememberRequest::new(P4_QUERY).unwrap();
    let out = v(remember(&tap, &tap, root.memory(), None, &req, "sess-p4", NOW).unwrap().text());
    assert_eq!(out["similar_n"], Value::Null, "0 会把「从没比过」读成「库里一条都不像」");
    assert_eq!(out["similar"], Value::Null);
    assert_eq!(tap.calls.lock().unwrap().len(), 1, "降级不许撤掉已经落下的那一行（P-61）");
    assert!(out["turn_uid"].is_string(), "回执本身仍然是成功的");
}

#[test]
fn 切不出词的正文那一次也是没比过() {
    // 坑 P-59②。`"！！！"` 过得了 `RememberRequest::new`（非空），但 `cut()` 会把纯标点滤干净
    // （本机已核 `rc-index/src/analyzer.rs` 的 `cut()` 里那支 `is_alphanumeric` 过滤）⇒ `NoQueryTokens`。
    // 索引在这条里**是建好的**（走 `card()`），所以唯一的失败来源是查询侧切不出词。
    let dir = tempfile::tempdir().unwrap();
    let _k = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let out = p4_out(dir.path(), "！！！");
    assert_eq!(out["similar_n"], Value::Null, "没法匹配被读成了没有匹配");
    assert_eq!(out["similar"], Value::Null);
}

#[test]
fn 回执顶层恒八格_分数与正文一格都不许出现() {
    // 三件事同一条（P-15 / P-46 那一族的写面版 / 守卫 12）：
    // ① 顶层格子**逐字**八格、字典序（`serde_json::Map` 是 `BTreeMap`，本机已核全仓没有
    //    `preserve_order` ⇒ 这个顺序是事实不是风格，与任务 11 的六格断言同一把尺子）；
    // ② `Scored` 的三个字段名一个都不许出现在线上；
    // ③ 卡片**正文**里的 canary 不许出现在回执里——`similar[]` 只许带 `id` 与 `标题`。
    let dir = tempfile::tempdir().unwrap();
    let _k = card(dir.path(), "先跑测试再合并", &format!("{CANARY} 任何合并之前先跑全量测试。"));
    let out = p4_out(dir.path(), P4_QUERY);
    let keys: Vec<String> = out.as_object().unwrap().keys().cloned().collect();
    assert_eq!(keys, vec!["accepted_chars", "eta_turns", "extract_state", "reason",
                          "schema_version", "similar", "similar_n", "turn_uid"]);
    let s = out.to_string();
    assert!(!s.contains(CANARY), "相似提示带了正文（守卫 12）：{s}");
    for w in ["score", "matched", "row_len"] {
        assert!(!s.contains(w), "内存里的整数尺子的字段名上了 agent 面：{w}");
    }
    // ②的另一半：那一格的**内容**也不许是分数。八格断言已经挡了新增格，
    // 这一句挡的是有人把 `"title"` 换成 `"0.87"` 之类的形状漂移。
    assert_eq!(out["similar"][0].as_object().unwrap().len(), 2, "similar 的一条多带了一格");
}

// ── W37-A1（G16 similar 旁路修复）：相似提示过读闸 ─────────────────────────

/// 一张带归属的卡：owner = (`agent`, `principal`)、private_agent（与生产者签名行同形）。
/// `put_card` 只会给 `OwnerCtx::synthetic`，带 principal 的归属要走 `put_batch_with_owner`。
fn card_owned(dir: &std::path::Path, agent: &str, principal: Option<&str>, title: &str)
    -> rc_memory::keys::MemKey
{
    use rc_memory::owner::{OwnerCtx, Visibility};
    use rc_memory::writers_x::{put_batch_with_owner, BatchCtx, WriteItem};
    let root = Root::open(dir).unwrap();
    let items = vec![WriteItem {
        table: rc_memory::MemTable::Card,
        kind: "decision".into(),
        title: title.into(),
        body: "任何合并之前先跑全量测试。".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        valid_to_ms: None,
        aliases: vec![],
        prov: vec![Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }],
    }];
    let k = put_batch_with_owner(
        root.memory(),
        &items,
        &BatchCtx { source_agent: agent.into(), device_id: "dev-g16".into() },
        &OwnerCtx {
            owner_principal_id: principal.map(String::from),
            owner_agent_id: agent.into(),
            visibility: Visibility::PrivateAgent,
            retention: "default".into(),
        },
        NOW,
        &[],
    ).unwrap().remove(0).expect("G16 夹具的行不被抑制");
    rc_index::rebuild(root.memory(), NOW).unwrap();
    k
}

/// 带指定读者的 `remember` 相似提示读数。
fn p4_out_as(dir: &std::path::Path, body: &str, viewer: Option<Viewer>) -> Value {
    let root = Root::open(dir).unwrap();
    let tap = Tap::fresh();
    let req = RememberRequest::new(body).unwrap();
    v(remember(&tap, &tap, root.memory(), viewer.as_ref(), &req, "sess-g16", NOW).unwrap().text())
}

#[test]
fn 相似提示过读闸_跨agent的行不再泄漏_id_与标题() {
    // G16（工单 W37-A1）：similar_hint 原来调无 viewer 的 rank ⇒ 跨 agent 泄 id+标题。
    // 修复后 similar 走读闸：agent-B(p2) 看不见 agent-A(p1) 的 private 行；
    // agent-B(p1) 经 4b 看得见；agent-B 无 principal 时 fail-closed，同样看不见。
    let dir = tempfile::tempdir().unwrap();
    let a = card_owned(dir.path(), "agent-A", Some("p1"), "先跑测试再合并");

    let stranger = p4_out_as(dir.path(), P4_QUERY, Some(Viewer::agent("agent-B").with_principal("p2")));
    assert_eq!(stranger["similar_n"], 0, "别的 principal 的 private 行泄进了相似提示：{stranger}");
    assert!(!stranger.to_string().contains(&a.key_string()), "泄漏了句柄：{stranger}");

    let anon = p4_out_as(dir.path(), P4_QUERY, Some(Viewer::agent("agent-B")));
    assert_eq!(anon["similar_n"], 0, "无 principal 的读者读到了别人的 private 行：{anon}");

    let mate = p4_out_as(dir.path(), P4_QUERY, Some(Viewer::agent("agent-B").with_principal("p1")));
    assert_eq!(mate["similar_n"], 1, "同 principal 的异 agent 没经 4b 读到：{mate}");
    assert_eq!(mate["similar"][0]["id"], a.key_string(), "{mate}");
}

// ── 任务 13（P5 落后与配额）：五条用例 + `Tap::with_health` ─────────────────

/// 各数各给各的，**不写成元组**：字段名是缝的公开形状，用例按名构造，
/// 将来加一格时这条 helper 是唯一要改的地方。`last_success_at_ms` 在这里固定给 `None`——
/// 它是缝最早那几格里唯一一个 `Option`，也就是"缝在场但那一格还没有生产者"只可能发生在
/// 那一格上（见第一条用例的注释），
/// 而它在真库上的非零形状由 `mcp/tests/p5_lag_quota.rs` 那条测。
/// 后来加入缝的字段（`last_derive_at_ms`、`derive_cursor`、`last_anomaly_scan_at_ms`）本身就是
/// `Option`，所以它们**由实参给**：`scan_at` 就是为 `status_心跳走缝` 那条用例开的——
/// 本文件测心跳的两种形状（缝搬得到 ⇒ 数、缝缺席 ⇒ `null`）必须能各喂一边，
/// 固定喂 `None` 就等于只测了半边。
/// `quota_stopped` 由实参给：S3a 起这一格是**裁定**（最近一次运行是否触顶），
/// 与 `used` 这个成本读数脱钩——两条用例要能各喂 `Some(false)` / `Some(true)` / `None`，
/// 而 `used` 固定喂 88 正是"累计不许代言判定"那半边的证人。
fn health(lag: i64, used: i64, scan_at: Option<i64>, stopped: Option<bool>) -> CycleHealth {
    CycleHealth { lag_turns: lag, last_success_at_ms: None,
                  llm_calls_used: used,
                  last_derive_at_ms: None, derive_cursor: None,
                  last_anomaly_scan_at_ms: scan_at,
                  quota_stopped: stopped, llm_calls_total: Some(used) }
}

#[test]
fn p5_三格在缝在场时两格是真值() {
    // `last_success_at_ms` 在 `CycleHealth` 里是 `Option`：缝在场不等于四个数都有生产者
    // （今天从没推进过那一轮，缝就递 `None` 进来）。于是"缝在场"这一格能读三种形状：
    // 真值、真值、`None`。第三格是 `None` 而不是 `0` 由这条用例钉住。
    let dir = tempfile::tempdir().unwrap();
    let _k = card(dir.path(), "先跑测试再合并", "任何合并之前先跑全量测试。");
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::default().with_health(Some(health(37, 12, None, Some(false))));
    let out = v(status(root.memory(), Some(&tap), &StatusOptions::default(), NOW).unwrap().text());
    assert!(out.as_object().unwrap().contains_key("distill_lag_turns"));
    assert_eq!(out["distill_lag_turns"], 37);
    assert_eq!(out["quota_exhausted"], false, "stopped=false ⇒ 没触顶，这一格必须是搬运而不是另算");
    assert_eq!(out["last_extract_at_ms"], Value::Null, "缝递 None 而 status 把它折成了 0：P-44 那一族");
    // 2026-11-06：本地调用配额砍掉 ⇒ status 的 JSON 里不许再出现"单轮上限"那一格。
    // 这一句是**形状**守卫：这一格从来没有过读者，但只要有人把它加回输出面（或把缝那一格
    // 加回来再接上），这里当场红。成本读数（`llm_calls_total`）与缝上的累加格 `llm_calls_used`
    // 都不受影响——上面那个 helper 仍在按名构造后者，删掉字段就编译不过。
    assert!(!out.as_object().unwrap().contains_key("llm_calls_limit"),
            "单轮上限已砍掉，status 里不许再有那一格");
    assert!(out.as_object().unwrap().contains_key("llm_calls_total"), "成本读数照旧在场");
}

#[test]
fn p5_配额耗尽只看最近一次运行_不看累计() {
    // S3a：`quota_exhausted` 的判定输入从"used >= limit"换成 `CycleHealth.quota_stopped`。
    // 三档各一次，**`used` 一律喂 88**（旧口径下它恒 true）：判定与累计必须当场分开。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::default().with_health(Some(health(0, 88, None, Some(false))));
    let out = v(status(root.memory(), Some(&tap), &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!(out["quota_exhausted"], false,
        "88 累计 + stopped=false ⇒ 不许报触顶（旧口径在这里恒 true）");
    assert_eq!(out["llm_calls_total"], 88, "累计另名成本读数，值照旧是那个和");
    assert_eq!(out["distill_lag_turns"], 0, "落后与配额是两格，一格不许替另一格说话");
    // 阳性：累计仍是 88，但运行记录触顶 ⇒ true（判定唯一来自运行记录）。
    let tap2 = Tap::default().with_health(Some(health(1, 88, None, Some(true))));
    let out2 = v(status(root.memory(), Some(&tap2), &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!(out2["quota_exhausted"], true, "stopped=true 而 status 不报触顶 ⇒ 判定丢了");
    // 三态：没有运行记录 ⇒ null，不是 false。
    let tap3 = Tap::default().with_health(Some(health(1, 88, None, None)));
    let out3 = v(status(root.memory(), Some(&tap3), &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!(out3["quota_exhausted"], Value::Null, "没有运行记录时这一格必须是 null");
}

#[test]
fn 缝缺席时三格是_null_而不是_0_与_false() {
    // P-54 的函数行为半边（装配半边在 hook/tests/p5_lag_quota.rs）。
    // 每格两句都在：键在场 + 值为 null。少一句就是"拼错键名也能绿"。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let out = v(status(root.memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    for k in ["distill_lag_turns", "last_extract_at_ms", "quota_exhausted"] {
        assert!(out.as_object().unwrap().contains_key(k), "键 {k} 不在场 ⇒ 两面的形状又不同了：{out}");
        assert_eq!(out[k], Value::Null, "{k} 在缝缺席时给了非 null 的读数，那它读的是什么？");
    }
    // 第四句是 P-54 后半段的对撞：`capabilities.extract_state` 仍然报 `queryable`。
    // "系统能力"与"本面可读"是两件事，前者不许被后者的 null 带跑。
    assert_eq!(out["capabilities"]["extract_state"], "queryable",
               "缝缺席把能力声明也一起降级了 ⇒ 那是第二个假读数");
    // 第五句是守卫 12 在三格上的延续：三格是计数与布尔，一个正文都不许带进来。
    assert!(!out.to_string().contains(CANARY), "P5 的格子里出现了正文：{out}");
}

#[test]
fn p5_的缝报错走既有分型_不新增错误码() {
    // P-61 的镜像面：`remember` 那一侧错误折成 null，`status` 这一侧错误**必须响亮**——
    // 因为 `status` 的成功与否**就是**"这套读数拿没拿到"，没有"已经写下去了"这个既成事实要保护。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::default(); // 没编排 health
    let e = status(root.memory(), Some(&tap), &StatusOptions::default(), NOW).unwrap_err();
    assert_eq!(e.kind, rc_agent::AgentErrorKind::Unavailable, "{e:?}");
    assert_eq!(e.code, "store_unreachable", "P5 的缝失败发明了第二个码：{}", e.code);
}

#[test]
fn remember_的回执格子数不因_p5_改变() {
    // §2.14 那句"P4/P5 走新增返回字段"里，**P5 的字段进的是 `status` 而不是 `remember`**。
    // 这一条防的是"顺手把三格也塞进 remember 回执"（那会破任务 12 刚钉死的恒等式，
    // 而 `tools_shape` 与 `p4_similar` 各有一条恒格数的断言——两条都会红，但红在这里更早）。
    // 键名按 `sort()` 比：`serde_json::Map` 是 `BTreeMap`（P-64 那条），但本仓的 sort 写法
    // 与任务 12 那条同形，两处不许一种是书写序一种是字典序。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    // `extraction` 与 `health` 两个编排位都给：前者让 `remember` 不撞 `turn_unreadable`，后者让
    // 这条用例真的在测 P5 的形状——若忘了给，回执顶多 4 格（`turn_unreadable` 提早返），
    // 那 8 格断言变成"长度对不上"，而不是"格子数被 P5 加多"。`Tap::fresh()` 已经把 `extraction`
    // 编好；这里只叠一个 `with_health`。
    let tap = Tap::fresh().with_health(Some(health(5, 5, None, Some(false))));
    let req = RememberRequest::new("任何合并之前先跑全量测试。").unwrap();
    let out = v(remember(&tap, &tap, root.memory(), None, &req, "sess-p5", NOW).unwrap().text());
    let mut keys: Vec<String> = out.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["accepted_chars", "eta_turns", "extract_state", "reason",
                      "schema_version", "similar", "similar_n", "turn_uid"],
               "remember 的回执长了 P5 的格子 ⇒ §2.14 的归属被改：{keys:?}");
}

// ── 任务 8（停轮与滞后两面可见）：两条用例 ─────────────────────────────────

#[test]
fn status_halted_scopes_and_lag_scopes_are_present_on_turn_path() {
    let tap = Tap::fresh();
    let tap = Tap { halted: 3, lag_scopes: vec![("dev-a/s-a".into(), 5), ("dev-b/s-b".into(), 2)], ..tap };
    let m = fresh_memory();
    let out = status(m, Some(&tap), &StatusOptions { turn: Some("0000000000000000000000000000000000000000000000000000000000000000"), ..Default::default() }, NOW).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out.text()).unwrap();
    assert_eq!(v["halted_scopes"], 3, "turn 路径上 halted_scopes 仍应出现");
    assert_eq!(v["lag_scopes"].as_array().unwrap().len(), 2);
    assert_eq!(v["lag_scopes"][0]["scope"], "dev-a/s-a", "按 lag 降序");
    assert_eq!(v["lag_scopes"][0]["lag"], 5);
    assert_eq!(v["lag_scopes_unlisted"], 0, "两 scope 全列出 ⇒ 未列出数是 0（真读数）");
}

#[test]
fn status_lag_scopes_unlisted_是截断后的余数() {
    // `Option<usize>` 的 Some 一支：父格在场时未列出数仍是"总数 − 截断上限"。
    let tap = Tap {
        lag_scopes: (0..MAX_STATUS_ENTITIES + 2)
            .map(|i| (format!("dev/s-{i}"), 1))
            .collect(),
        ..Tap::fresh()
    };
    let m = fresh_memory();
    let out = status(m, Some(&tap), &StatusOptions::default(), NOW).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out.text()).unwrap();
    assert_eq!(v["lag_scopes"].as_array().unwrap().len(), MAX_STATUS_ENTITIES,
        "列表按上限截断");
    assert_eq!(v["lag_scopes_unlisted"], 2, "余数 = 总数 − 截断上限");
}

#[test]
fn status_halted_scopes_and_lag_scopes_are_null_when_no_read() {
    let m = fresh_memory();
    let out = status(m, None, &StatusOptions::default(), NOW).unwrap();
    let v: serde_json::Value = serde_json::from_str(&out.text()).unwrap();
    assert!(v["halted_scopes"].is_null(), "缝缺席 ⇒ halted_scopes = null");
    assert!(v["lag_scopes"].is_null(), "缝缺席 ⇒ lag_scopes = null");
    // S3b：未列出数是父格的派生读数，父格缺席时它必须跟着 `null`，不许折成 0——
    // `0` 会被读成"列出了全部、一条没剩"，而这一面其实什么都没读到（P-54 的假零）。
    assert!(v["lag_scopes_unlisted"].is_null(),
        "lag_scopes 缺席 ⇒ lag_scopes_unlisted = null，不是 0");
}

#[test]
fn status_attention_open_是数_缝缺席也是数() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let out = v(status(root.memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    assert!(out.as_object().unwrap().contains_key("attention_open"),
        "键恒在场（P-54），缝缺席不许把键吞掉");
    assert_eq!(out["attention_open"], 0, "空库 + 缝缺席 ⇒ 恒为 0，不许是 null");
}

#[test]
fn status_attention_open_数得出真告警() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let uid = "t-shape-anchor".to_string();
    let k = put_card(m, &FactContent {
        kind: "note".into(), title: "死后又被确认".into(),
        body: format!("{CANARY} 合成正文，不含任何生产文本"),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: uid.clone(), anchor_role: AnchorRole::Human }], NOW).unwrap();
    // 条款②的生产形状：先判失败，退场，再让正面表态解除退场账（C24-N1）——
    // 行复活但 verdict='failed' 仍在，sweep 才会升这一条。
    set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
    retire(m, &k, "manual", NOW + 1).unwrap();
    attest(m, &k, Stance::Confirmed, Some(&uid), None, NOW + 10).unwrap();
    let rep = rc_memory::anomaly::sweep(m, NOW + 20).unwrap();
    assert_eq!(rep.raised, 1, "前置：sweep 真的升了一条，否则下面那个 1 是空断言");
    let out = v(status(m, None, &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!(out["attention_open"], 1, "sweep 落的账必须被这一格数到");
    assert!(!out.to_string().contains(CANARY), "告警格不许把正文带上运维面");
}

#[test]
fn status_心跳走缝_缺席为_null_在场为数() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let tap = Tap::default().with_health(Some(health(37, 12, Some(NOW), Some(false))));
    let out = v(status(root.memory(), Some(&tap), &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!(out["last_anomaly_scan_at_ms"], NOW, "缝搬得到的心跳要出数");

    let out2 = v(status(root.memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    assert!(out2.as_object().unwrap().contains_key("last_anomaly_scan_at_ms"), "键恒在场（P-54）");
    assert_eq!(out2["last_anomaly_scan_at_ms"], Value::Null,
        "缝缺席 ⇒ null（与 last_derive_at_ms 同族，与 attention_open 正好相反）");
}

#[test]
fn status_recheck_pending_与_attention_open_各数各的() {
    // 复核只把"待复核"降下去，**不许**把"未解决告警"那一格一起降。
    // 牙：`SET rechecked_at_ms = ?1` 写成 `SET rechecked_at_ms = ?1, resolved_at_ms = ?1`
    // ⇒ 盖戳顺手解除告警 ⇒ `attention_open` 变 0、这一条红（变异 ③ 有两个证人）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let uid = "t-shape-recheck".to_string();
    let k = put_card(m, &FactContent {
        kind: "note".into(), title: "死后又被确认".into(),
        body: "一条合成正文，不含任何生产文本".into(),
        scope: Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() },
        valid_to_ms: None,
    }, &[Provenance { turn_uid: uid.clone(), anchor_role: AnchorRole::Human }], NOW).unwrap();
    // 条款②的生产形状：先判失败，退场，再让正面表态解除退场账（C24-N1）——
    // 行复活但 verdict='failed' 仍在，sweep 才会升这一条。
    set_verdict(m, &k, Verdict::Failed, NOW).unwrap();
    retire(m, &k, "manual", NOW + 1).unwrap();
    attest(m, &k, Stance::Confirmed, Some(&uid), None, NOW + 10).unwrap();
    rc_memory::anomaly::sweep(m, NOW + 20).unwrap();

    let before = v(status(m, None, &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!((before["attention_open"].as_i64(), before["recheck_pending"].as_i64()),
               (Some(1), Some(1)), "盖戳之前两格都是 1（都不是 null）：{before}");

    attest(m, &k, Stance::Corroborated, Some(&uid), None, NOW + 30).unwrap();
    assert_eq!(rc_memory::recheck::recheck_attested(m, NOW + 31).unwrap(), 1);
    let after = v(status(m, None, &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!((after["attention_open"].as_i64(), after["recheck_pending"].as_i64()),
               (Some(1), Some(0)), "盖过戳：待复核归 0，未解决告警照旧是 1：{after}");
}

/// C25-N4：读失败必须被报出来，不许折成"库里没有"。
/// 注入在**独立的** AgentRoot 上：本文件的共享夹具被并行用例共用，拿掉一张表会让别人看见半个库。
#[test]
fn a_failing_memory_read_is_reported_not_swallowed() {
    let dir = tempfile::tempdir().unwrap();
    let root = AgentRoot::open(dir.path()).unwrap();
    root.memory()
        .conn()
        .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE card;")
        .unwrap();
    let e = status(root.memory(), None, &StatusOptions::default(), NOW)
        .expect_err("读失败必须是一个错误，不是一份空读数");
    assert_eq!(e.code, "store_unreachable", "{e:?}");
    assert!(matches!(e.kind, AgentErrorKind::Unavailable), "{e:?}");
}

// ── P0-1：身份通道的两格读数 ────────────────────────────────────────────────

/// 缝缺席时 `principal_claims` 是 `null`（键恒在场，P-54），而记忆库那一格**恒为数**——
/// 两格的来路不同（一个要转录柄、一个直读记忆库），所以"没有缝"不许让后者也变 null。
#[test]
fn p0_1_两格在缝缺席时_章数为_null_事实计数仍可读() {
    let out = v(status(fresh_memory(), None, &StatusOptions::default(), NOW).unwrap().text());
    for key in ["principal_claims", "facts_with_agent_speaker_but_human_evidence"] {
        assert!(out.as_object().unwrap().contains_key(key), "键 {key} 不在场：{out}");
    }
    assert_eq!(out["principal_claims"], Value::Null,
        "缝缺席折成 0 会被读成「一个会话都没盖章」");
    assert_eq!(out["facts_with_agent_speaker_but_human_evidence"], 0,
        "空库上「一条都没有」是真读数 0，不是读不到");
}

/// 缝在场时章数原样透出；同一份读数里两格互不顶替。
#[test]
fn p0_1_缝在场时_章数是搬运值() {
    let tap = Tap { principal_claims: Some(3), ..Tap::fresh() };
    let out = v(status(fresh_memory(), Some(&tap), &StatusOptions::default(), NOW).unwrap().text());
    assert_eq!(out["principal_claims"], 3);
    assert_eq!(out["facts_with_agent_speaker_but_human_evidence"], 0);
}
