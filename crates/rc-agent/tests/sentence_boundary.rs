//! 全文档（`Form::Full`）的**断句判据**（Task 14 / D8）。
//!
//! 方法论逐字沿用 `block_cap.rs` 的两条注释：
//! ① 渲染素材从真库里取——正文走 `put_card` 播种、再由 `reader::lookup` 读回 `RowFacts`，
//!    不在内存里手编一个"看起来像渲染层输入"的结构；
//! ② `ProbeView` 手工拼，`forms` **未经 `fit`**——用 `probe_via_search` 拿到的东西测渲染层的
//!    剪刀永远绿，它在进渲染层之前就被截好了。
//!
//! 断言的对象是 `probe_block` 渲染出的那一行，**不是** `surface::first_sentence`：那是私有函数，
//! 集成测试够不着，而把它改 `pub` 是给渲染层新增公开 API（撞守卫 12——`surface` 是围栏与阶梯
//! 渲染的唯一家）。

use rc_agent::surface::probe_block;
use rc_agent::FENCE;
use rc_memory::keys::MemKey;
use rc_memory::reader::RowFacts;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Verdict};
use rc_search::gate::{Cells, Decision, Form, ProbeView};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() }
}

/// 真库播种一条正文，再 `lookup` 读回渲染层真正吃的那个 `RowFacts`。
/// 句柄是内容的函数（id 由 preimage 算出）⇒ 一条正文一个 id，所以本文件不需要"猜 id"，
/// 也不需要第二个查询键：`put_card` 交回来的那一个就是它。
fn facts_with_body(dir: &std::path::Path, body: &str) -> (MemKey, RowFacts) {
    let root = Root::open(dir).unwrap();
    let k = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: "断句夹具的标题".into(),
            body: body.into(),
            scope: scope(),
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "e".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    // full 档在生产判定里只由可核查证据打开；本文件走 `raw_full` 不经判定层，`set_verdict`
    // 带的是把这条夹具钉在"真有 full 资格的那一档"上——哪天渲染器改分档，这里会当场红，
    // 而不是悄悄变成对别的档的一次空测。
    rc_memory::set_verdict(root.memory(), &k, Verdict::Passed, NOW).unwrap();
    let facts = rc_memory::reader::lookup(root.memory(), &k).unwrap().unwrap();
    assert_eq!(facts.body, body, "夹具正文进库后变了样 ⇒ 下面断的就不是这条正文");
    (k, facts)
}

/// 手工拼一个只装这一条 `Form::Full` 的 `ProbeView`（`forms` 未经 `fit`）。
fn raw_full(key: MemKey, facts: RowFacts) -> ProbeView {
    let decision = Decision { key, form: Form::Full, cell: None, score: 1024, facts };
    ProbeView { forms: vec![decision], silent: Cells::default(), candidates: 1 }
}

/// 一条正文经渲染器出来的**两个物理行**（`surface.rs` 的 full 臂：正文行 + `句柄 | as_of=` 行）。
/// 形状前提（行数、围栏、第二行逐字）在这里一次守住，调用方只比正文那一行——
/// 否则"两行形状被改掉"会伪装成"断句被改掉"，红的是错的那件事。
fn rendered_full(dir: &std::path::Path, body: &str) -> Vec<String> {
    let (k, facts) = facts_with_body(dir, body);
    let text = probe_block(&raw_full(k.clone(), facts)).text();
    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
    assert_eq!(lines.len(), 2, "full 档不是两个物理行 ⇒ 本文件的判据无处落脚：{lines:?}");
    assert!(
        lines.iter().all(|l| l.starts_with(FENCE)),
        "围栏不是每行前缀（V3）：{lines:?}"
    );
    assert_eq!(lines[1], format!("{FENCE}{} | as_of={}", k.key_string(), NOW), "第二行不是句柄 + 时间：{lines:?}");
    lines
}

/// 全文档是设计里唯一"代言"的一档，注入残句等于让 agent 说错话。
/// 型号名里的 ASCII 点不是句末（真值形状取自本机库 21/35 张卡）。
#[test]
fn 型号名里的点不当句末() {
    let dir = tempfile::tempdir().unwrap();
    for body in [
        "甲某随行要求测试Foo3.8-27B能否用于recollect的蒸馏和审核。",
        "换到 DeepSeek-V4.1-Flash 之后延迟降了。",
        "用 GLM-5.3-Flash 跑一遍。",
    ] {
        let lines = rendered_full(dir.path(), body);
        assert_eq!(lines[0], format!("{FENCE}{body}"), "被切残了：{body}");
    }
}

/// 阳性对照：真句末仍然断句，别把上一条放宽成"永不断句"。
/// 四臂各钉一条边界规则（规则表见 `surface.rs`），少一臂就意味着那一臂被放宽过头。
#[test]
fn 真句末仍然断句() {
    let dir = tempfile::tempdir().unwrap();
    for (body, want) in [
        // 中文句末标点：与 ASCII 点无关，永远断。
        ("第一句。第二句不该出现。", "第一句。"),
        // ASCII 句末 + 空格 + 数字：点左侧是**字母**、右侧是空白 ⇒ 不在无空白 token 内部 ⇒ 该断。
        // 「右侧跟着数字」不是豁免理由：把空白判据从「两侧都非空白」放宽成「任一侧非空白」，
        // 这一臂当场红（实测，见交付报告 §6 变异 M4）。
        ("见 p. 12 页与后面那句。", "见 p."),
        // ASCII 句末 + 空格 + 非数字（下一个 token 以字母开头）：同样该断。
        ("见 Q. 下一句不该出现。", "见 Q."),
        // ASCII 句末 + 空格 + ASCII 词元：西文句末的常见形状。
        ("The gate opens. The second sentence must not appear.", "The gate opens."),
    ] {
        let lines = rendered_full(dir.path(), body);
        assert_eq!(lines[0], format!("{FENCE}{want}"), "真句末没断句：{body}");
    }
}

/// "两侧都是数字"那一臂单独点名，并配上它的反面"数字 + 点 + 空白"。
/// 两臂合起来才钉住"数字"这一维：只给前一条，实现可退化成"永不断句"（上一条与第 2 臂当场红）；
/// 只给后一条，实现可退化成"永远断句"（第 1 条与第 1 臂当场红）。
/// 第 3 臂（`字母 + 点 + 中文`）是判据②的**唯一读者**：把②摘掉只留①时，前两处测试全绿、
/// 只有这一臂红——所以②不是①的冗余（实测见交付报告 §6 变异 M3）。
#[test]
fn 点两侧的形态决定断不断() {
    let dir = tempfile::tempdir().unwrap();
    // 数字 + 点 + 数字（版本号、小数）：不许断。
    let lines = rendered_full(dir.path(), "先把 3.8 换掉再说。后面那句不该出现。");
    assert_eq!(lines[0], format!("{FENCE}先把 3.8 换掉再说。"), "数字两侧的点被当成了句末：{lines:?}");
    // 数字 + 点 + 空白（真句末）：该断。
    let lines = rendered_full(dir.path(), "共 7. 剩下的不该出现。");
    assert_eq!(lines[0], format!("{FENCE}共 7."), "数字后面的真句末没断：{lines:?}");
    // 型号名 + 点 + 中文（无空格）：按判据属于"点在一个不含空白的 token 内部" ⇒ 不许断，
    // 断点落到后面那个中文句末。这一臂钉的是判据的**代价**被写进契约：
    // ASCII 点直接贴着中文时不再断开，那一整串靠中文标点或 120 字符上限收口。
    let lines = rendered_full(dir.path(), "改用 MiMo-V2.6-Flash.它比前一档稳。");
    assert_eq!(lines[0], format!("{FENCE}改用 MiMo-V2.6-Flash.它比前一档稳。"), "非空白的点断错了位置：{lines:?}");
}
