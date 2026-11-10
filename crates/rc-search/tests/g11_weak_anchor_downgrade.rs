//! G11（2026-11-07 召回审计 + 波次 W35）：弱锚格的降级出声。
//!
//! 审计读数：hook 注入面被 `WeakAnchor` 一格吞掉约 2/3 探针——126 卡多数出处仅 agent 轮
//! （`strong_anchor = anchor_role == Human`），真正复核通过（`verdict=Passed`）的事件卡也被吞。
//! 拍板（只动 `decide()` 的 WeakAnchor 格，判定顺序与格子归属一字不动）：
//! `Passed && kind=="event"` 的卡从 `Form::Silent` 降为 `Form::Signpost`（一行出声），
//! 其余弱锚行照旧沉默；**格子照旧记 `WeakAnchor`**——cells 是"判定落点"诊断，
//! form 才是出声形态。fact/state 的弱锚行不进白名单，等读数。
//!
//! 夹具全部合成（守卫 12）：正文是编造的中文短句，没有 CANARY、没有路径、没有密钥。

use rc_memory::{
    put_card, set_verdict, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Verdict,
};
use rc_search::gate::{cells_of, decide, fit, Form, GateCell};
use rc_search::rank::{rank, Scored};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;
/// 查询词与正文同词：命中数不是 1 就红的口径与 `gate_switches.rs::only` 相同。
const QUERY: &str = "弱锚降级用例 合成查询词";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-g11".into() }
}
fn agent_prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "a".repeat(64), anchor_role: AnchorRole::Agent }]
}
fn human_prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }]
}
fn content(kind: &str, title: &str) -> FactContent {
    FactContent {
        kind: kind.into(),
        title: title.into(),
        body: "弱锚降级用例 合成查询词 一句足够短的话".into(),
        scope: scope(),
        valid_to_ms: None,
    }
}
/// `gate_switches.rs::memory_handle` 的同一口径：`TempDir` 先于句柄活着、由调用方绑定。
fn memory_handle(dir: &std::path::Path) -> MemoryHandle {
    Root::open(dir).unwrap().into_memory()
}
/// 按标题取唯一候选；命中数不是 1 意味着夹具污染了别的行。
fn scored(dir: &std::path::Path, title: &str) -> Scored {
    let m = memory_handle(dir);
    let mut got: Vec<_> = rank(&m, None, NOW, QUERY)
        .unwrap()
        .into_iter()
        .filter(|s| s.facts.title == title)
        .collect();
    assert_eq!(got.len(), 1, "标题 {title} 命中 {} 行，夹具被污染", got.len());
    got.pop().unwrap()
}

/// 测试钉①：Passed+event+弱锚 ⇒ Signpost（1 行、cell=WeakAnchor、cells 统计含它）。
#[test]
fn g11_passed_event_弱锚卡降级为_signpost_一行出声且格子照旧记_weak_anchor() {
    let dir = tempfile::tempdir().unwrap();
    {
        let m = memory_handle(dir.path());
        let k = put_card(&m, &content("event", "弱锚事件卡"), &agent_prov(), NOW).unwrap();
        set_verdict(&m, &k, Verdict::Passed, NOW).unwrap();
        rc_index::rebuild(&m, NOW).unwrap();
    }
    let s = scored(dir.path(), "弱锚事件卡");
    // 夹具回执：弱锚、Passed、event 三件事都必须真的成立，否则下面的断言在测别的东西。
    assert_eq!(s.facts.anchor_role, AnchorRole::Agent, "夹具不是弱锚");
    assert_eq!(s.facts.verdict, Verdict::Passed, "夹具不是 Passed");
    assert_eq!(s.facts.kind, "event", "夹具不是 event 卡");

    let d = decide(&s, NOW);
    assert_eq!(d.form, Form::Signpost, "Passed 的 event 卡不许被弱锚格整行吞掉（G11）");
    assert_eq!(d.lines(), 1, "Signpost 一行（fit/lines 一字不动的行为面）");
    assert_eq!(
        d.cell,
        Some(GateCell::WeakAnchor),
        "格子照旧记 WeakAnchor：cells 是判定落点诊断，form 才是出声形态"
    );
    // cells 统计含它：单行判定落进 WeakAnchor 一格，其余四格为 0。
    let cells = cells_of(std::slice::from_ref(&d));
    let expected = GateCell::ALL
        .map(|c| (c, if c == GateCell::WeakAnchor { 1 } else { 0 }));
    assert_eq!(cells.counts(), expected, "cells 统计没把降级行记进 WeakAnchor");
    // fit 形状：出声的那一行占 1 行额度，进得去截断面。
    let fitted = fit(std::slice::from_ref(&d));
    assert_eq!(fitted.len(), 1, "降级出声的行必须进 fit 的产出");
}

/// 测试钉②：Passed+非 event kind+弱锚 ⇒ Silent（白名单只收 event，fact/state 等读数）。
#[test]
fn g11_passed_非_event_弱锚卡照旧_silent() {
    let dir = tempfile::tempdir().unwrap();
    {
        let m = memory_handle(dir.path());
        let k = put_card(&m, &content("decision", "弱锚决策卡"), &agent_prov(), NOW).unwrap();
        set_verdict(&m, &k, Verdict::Passed, NOW).unwrap();
        rc_index::rebuild(&m, NOW).unwrap();
    }
    let s = scored(dir.path(), "弱锚决策卡");
    assert_eq!(s.facts.anchor_role, AnchorRole::Agent);
    assert_eq!(s.facts.verdict, Verdict::Passed);
    assert_eq!(s.facts.kind, "decision");
    let d = decide(&s, NOW);
    assert_eq!(d.form, Form::Silent, "非 event 的 Passed 弱锚卡不许出声");
    assert_eq!(d.cell, Some(GateCell::WeakAnchor));
}

/// 测试钉③：非 Passed（Unreviewed / Low）+弱锚 ⇒ Silent。
///
/// 第三臂"Confirmed 单次"在本仓**结构上造不出来**：正面表态必须挂在该行的人工轮出处上
/// （`rc-memory/src/writers_a.rs:19-26` 对 `AnchorNotHuman` 的早退），而带人工出处的行
/// 就不是弱锚了——与 `gate_switches.rs::合取性质` 那一臂的注记同一个实测结论。
#[test]
fn g11_非_passed_弱锚_event_卡照旧_silent() {
    let dir = tempfile::tempdir().unwrap();
    {
        let m = memory_handle(dir.path());
        let k1 = put_card(&m, &content("event", "未审事件卡"), &agent_prov(), NOW).unwrap();
        let k2 = put_card(&m, &content("event", "低评事件卡"), &agent_prov(), NOW).unwrap();
        set_verdict(&m, &k2, Verdict::Low, NOW).unwrap();
        rc_index::rebuild(&m, NOW).unwrap();
        let _ = (&k1, &k2);
    }
    for (title, v) in [("未审事件卡", Verdict::Unreviewed), ("低评事件卡", Verdict::Low)] {
        let s = scored(dir.path(), title);
        assert_eq!(s.facts.verdict, v, "{title} 夹具判决不对");
        let d = decide(&s, NOW);
        assert_eq!(d.form, Form::Silent, "{v:?} 的弱锚 event 卡不许出声");
        assert_eq!(d.cell, Some(GateCell::WeakAnchor), "{v:?} 归错格");
    }
}

/// 测试钉④：强锚+Passed 仍 Full（full 路径一字不动；白名单不收窄强锚面）。
#[test]
fn g11_强锚_passed_仍_full_full_路径一字不动() {
    let dir = tempfile::tempdir().unwrap();
    {
        let m = memory_handle(dir.path());
        let k = put_card(&m, &content("event", "强锚事件卡"), &human_prov(), NOW).unwrap();
        set_verdict(&m, &k, Verdict::Passed, NOW).unwrap();
        rc_index::rebuild(&m, NOW).unwrap();
    }
    let s = scored(dir.path(), "强锚事件卡");
    assert_eq!(s.facts.anchor_role, AnchorRole::Human, "夹具不是强锚");
    let d = decide(&s, NOW);
    assert_eq!(d.cell, None, "强锚 Passed 行不该落任何格");
    assert_eq!(d.form, Form::Full, "full 路径被 G11 波及了");
    assert_eq!(d.lines(), 2);
}
