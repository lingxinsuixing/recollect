//! 分节函数 `section_of` 的逐格断言（Phase 2 / 任务 8）。
//!
//! **为什么从渲染面断而不是直接调 `section_of`**：`section_of` 是本模块的私有函数，
//! 而派发档的文件集只许改它的 match 臂（可见性也是契约的一部分，不许为了测试把它抬成 `pub`）。
//! 于是这里走消费面：`view=brief` 的分节标题 `▍<分节名>` 就是 `section_of` 的唯一读者可见产物，
//! 一张卡出现在哪个标题下，它就落进了哪个分节。这样断到的还是**真那条链**
//! （`recall.rs:596` 的 `section: section_of(&f.kind)`），不是它的复制品。
//!
//! **为什么逐格断、不许用总称盖**（`feedback-coverage-claim-needs-per-item-assertion`）：
//! 一族 N 项用"都不落其他"这种总称盖住，会静默漏掉其中一格——下面 `CELLS` 十条
//! 各自 `assert_eq!` 到期望分节名，摘掉任意一条 match 臂只有对应那一格变红。

use rc_agent::recall::{recall_narrative, NarrativeView, RecallFilter, RecallRequest};
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

/// 生产侧实际会写进 `card.kind` 的十个值 → 期望分节。
///
/// 词表的家（不是这里造的）：
/// - `crates/rc-memory/src/writers_fact.rs:35` 的 `FACT_CATEGORIES` 七值
///   `event/promise/plan/decision/observation/state_change/other`——`put_fact_rows` 把
///   `it.category` 逐字当 `card.kind` 写进 `CARD_INSERT`；
/// - `crates/rc-memory/src/writers_x.rs` 两条推导 INSERT 的字面量 `'timeline'` / `'causal'`；
/// - 派发档 I-3a 交付 4：`writers_x.rs` 的 `put_episodes` 第三条推导 INSERT 的字面量
///   `'episode'`（episode 的稳定消费句柄就是这张镜像卡，`MemTable` 不扩）。
///
/// **`other` 期望落「其他」是刻意的处置，不是漏网**：它是写侧的残值兜底
/// （`writers_fact.rs:34` 的注释原话 "`other` 是残值兜底"），读者侧的「其他」就是它的同形对应格。
/// 把它塞进任何一个有内容的分节（最像的是"笔记与经验"），等于替它宣称"这是一条笔记/经验"——
/// 而"这条记忆该算哪一类"正是写侧没能判定的那件事。残值对残值，语义才是诚实的。
/// 所以判据写成 **九格有分节 + 第十格明示留在「其他」**（派发档 Step 1 给的两个选项里取前者）。
///
/// 分节名逐字取自 `recall.rs` 的 `section_of`，**没有新增分节名**：
/// 人物与关系 / 事件与经历 / 状态与偏好 / 决定与承诺 / 笔记与经验 / 其他。
const CELLS: [(&str, &str); 10] = [
    ("event", "事件与经历"),
    ("timeline", "事件与经历"),
    ("causal", "事件与经历"),
    ("episode", "事件与经历"),
    ("observation", "事件与经历"),
    ("promise", "决定与承诺"),
    ("plan", "决定与承诺"),
    ("decision", "决定与承诺"),
    ("state_change", "状态与偏好"),
    ("other", "其他"),
];

/// 十张卡，每 kind 一张，句柄臂渲染 brief（句柄臂不排序、不截断，比查询臂确定）。
fn brief_with_ten_kinds() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let mut keys = Vec::new();
    for (kind, _) in CELLS {
        let key = put_card(
            m,
            &FactContent {
                kind: kind.into(),
                title: format!("分节夹具{kind}"),
                body: format!("{kind}这一格的夹具正文。"),
                scope: Scope { kind: ScopeKind::Project, key: "proj-section-kinds".into() },
                valid_to_ms: None,
            },
            &[Provenance {
                turn_uid: format!("{:02}{}", keys.len(), "a".repeat(62)),
                anchor_role: AnchorRole::Human,
            }],
            NOW,
        )
        .unwrap();
        keys.push(key);
    }
    let text = recall_narrative(
        m,
        &RecallRequest::Handles(keys),
        &RecallFilter::empty(),
        NOW,
        NarrativeView::Brief,
    )
    .unwrap()
    .text();
    (dir, text)
}

/// 一张卡（按标题认）落在哪个分节标题下。渲染形状的唯一依据：`assemble_brief` 里
/// 分节行是 `▍<分节名>`，条目行是 `- <text>（<when>）。[n]`。
fn section_holding(text: &str, title: &str) -> Option<String> {
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix('▍') {
            current = Some(rest.to_string());
        } else if line.starts_with("- ") && line.contains(title) {
            return current;
        }
    }
    None
}

/// 十个 kind 各自归到指定分节——**逐格**断，且先数到 10：
/// 任何一格没在简报里出现（被预算丢掉 / 句柄读不到）都算这一格红。
#[test]
fn 生产侧十个_kind_逐格归位() {
    let (_d, text) = brief_with_ten_kinds();
    let mut rendered = 0usize;
    for (kind, want) in CELLS {
        let title = format!("分节夹具{kind}");
        let got = section_holding(&text, &title);
        assert!(
            got.is_some(),
            "{kind} 这一格压根没被渲染出来（少了它下面的逐格断言全是空转）：{text}"
        );
        rendered += 1;
        assert_eq!(
            got.as_deref().unwrap(),
            want,
            "{kind} 应归「{want}」，实际落在别的分节里：{text}"
        );
    }
    assert_eq!(rendered, 10, "必须逐格数到 10，不许有一格缺席");
}

/// 除残值 `other` 之外，生产侧那九个 kind 一个都不许落进「其他」——
/// 这是缺陷本身的那一条判据（修复前 `promise`/`observation`/`state_change` 三格在此红）。
#[test]
fn 除残值other外_生产侧九个_kind_无一落进其他() {
    let (_d, text) = brief_with_ten_kinds();
    let mut fell_through: Vec<&str> = Vec::new();
    let mut scanned = 0usize;
    for (kind, want) in CELLS {
        if want == "其他" {
            continue;
        }
        scanned += 1;
        if section_holding(&text, &format!("分节夹具{kind}")).as_deref() == Some("其他") {
            fell_through.push(kind);
        }
    }
    assert_eq!(scanned, 9, "判据扫描的是九格（第十格 other 是残值），少了就不对");
    assert!(
        fell_through.is_empty(),
        "这 {}/9 个 kind 落进了「其他」：{fell_through:?}",
        fell_through.len()
    );
}

/// 残值那一格自己也要有读者：`other` 必须**确实**落「其他」，不许被顺手塞进有内容的分节。
/// 上面 `CELLS` 已经断过一次，这里是把它单独点名——免得日后有人为了"十个都不落其他"
/// 把 `other` 也加进某条臂，那会让"写侧没判定"这件事在读者侧伪装成"这是一条笔记"。
#[test]
fn 残值other明示留在其他分节() {
    let (_d, text) = brief_with_ten_kinds();
    assert_eq!(
        section_holding(&text, "分节夹具other").as_deref(),
        Some("其他"),
        "other 是写侧残值兜底，读者侧的同形对应格就是「其他」：{text}"
    );
}
