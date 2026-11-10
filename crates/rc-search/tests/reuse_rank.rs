//! 波二刀 B：复用反馈在**读取侧**的两臂（设计 `2026-11-08-g1-reuse-lesson-design.md`
//! §波次二第 3 条；派发档测试钉①②③）。
//!
//! 三格钉：
//! 1. `worked >= 2` 的卡升一档——同分对照下排到前面（信号不靠改 `score`，见 `rank.rs`
//!    的 `ReuseSignal` 文档）；
//! 2. `worked == 1` **不加成**（与没有反馈的对照逐位同序）；
//! 3. `stale` 反馈进 `stale` 判定：`decide` 落 `GateCell::Stale`、`probe_view` 不出声，
//!    而 `rank` 照旧返回它（过滤在闸门，不在检索——G6 判据③）。
//!
//! 夹具全部合成（守卫 12）。

use rc_memory::keys::MemKey;
use rc_memory::reuse::record_reuse;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_search::gate::{decide, probe_view, Form, GateCell};
use rc_search::rank::{rank, Scored};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_500_000;
const QUERY: &str = "复用升档合成词";
const BODY: &str = "复用升档合成词 一句足够短的正文";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-reuse-rank".into() }
}

fn human_prov() -> Vec<Provenance> {
    vec![Provenance { turn_uid: "h".repeat(64), anchor_role: AnchorRole::Human }]
}

fn memory_handle(dir: &std::path::Path) -> MemoryHandle {
    Root::open(dir).unwrap().into_memory()
}

/// 两张**同分**卡：标题只差末字（各一词），正文完全相同 ⇒ 命中词数与行长都相同。
/// 返回 `(甲卡, 乙卡)` 的句柄；两张卡的内容寻址与目录无关，两个夹具目录得到同一对 id。
fn seed_pair(m: &MemoryHandle) -> (MemKey, MemKey) {
    let a = put_card(
        m,
        &FactContent { kind: "note".into(), title: "甲卡".into(), body: BODY.into(), scope: scope(), valid_to_ms: None },
        &human_prov(),
        NOW,
    )
    .unwrap();
    let b = put_card(
        m,
        &FactContent { kind: "note".into(), title: "乙卡".into(), body: BODY.into(), scope: scope(), valid_to_ms: None },
        &human_prov(),
        NOW,
    )
    .unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    (a, b)
}

fn order(m: &MemoryHandle) -> Vec<String> {
    rank(m, None, NOW, QUERY).unwrap().into_iter().map(|s| s.key.key_string()).collect()
}

fn by_key(m: &MemoryHandle, key: &MemKey) -> Scored {
    rank(m, None, NOW, QUERY)
        .unwrap()
        .into_iter()
        .find(|s| &s.key == key)
        .unwrap_or_else(|| panic!("{} 不在候选池", key.key_string()))
}

/// 钉①：`worked >= 2` 的卡（这里是内容寻址下**字典序更大**的乙卡）在**同分**对照下上移到首位。
#[test]
fn worked_两次的卡在同分对照下上移() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let (a, b) = seed_pair(&m);
    // 对照：没有反馈时按 (分数, 句柄) 排；两卡同分 ⇒ 只按句柄。
    let control = order(&m);
    assert_eq!(control.len(), 2, "夹具应恰好两张卡：{control:?}");
    // 选字典序更大那张盖两次 worked：没有升档时它必然排在后面（"上移"才是信号）。
    let (boosted, other) = if a.key_string() > b.key_string() { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
    assert_eq!(control[0], other.key_string(), "对照前提破了：同分时字典序小的必须在前");
    record_reuse(&m, &boosted.id, "worked", None, NOW, "agent-reuse-rank").unwrap();
    record_reuse(&m, &boosted.id, "worked", None, NOW, "agent-reuse-rank").unwrap();
    let after = order(&m);
    assert_eq!(after[0], boosted.key_string(), "worked=2 的卡必须上移到首位：{after:?}");
    assert_eq!(after[1], other.key_string());
    // 分数本身一字不动：升档是分层，不是给 score 加常数（公式不变量仍然成立）。
    let sa = by_key(&m, &boosted);
    let sb = by_key(&m, &other);
    assert_eq!(sa.score, sb.score, "同分对照破了：两卡相关度不同就没在测升档");
    assert_eq!(sa.reuse.worked, 2);
    assert_eq!(sa.reuse.worked_tier(), 1, "两档：worked>=2 才升");
    assert_eq!(sb.reuse.worked_tier(), 0);
}

/// 钉②：`worked == 1` 不加成——排序与"完全没有反馈"的对照逐位相同。
#[test]
fn worked_一次不加成() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let (a, b) = seed_pair(&m);
    let before = order(&m);
    let target = if a.key_string() > b.key_string() { a.clone() } else { b.clone() };
    record_reuse(&m, &target.id, "worked", None, NOW, "agent-reuse-rank").unwrap();
    let after = order(&m);
    assert_eq!(before, after, "worked=1 不许改变顺序");
    let s = by_key(&m, &target);
    assert_eq!(s.reuse.worked, 1);
    assert_eq!(s.reuse.worked_tier(), 0, "阈值钉死：worked=1 不升档");
}

/// 钉③：`stale` 反馈进**读取侧 stale 判定**——`decide` 落 `GateCell::Stale` 且沉默，
/// hook probe 的出声面不出现它；而 `rank` 照旧返回（检索不做过滤）。
///
/// 形状理由（派发档"三选一或组合，报告写选择理由"）：选**闸格**而不是（a）直接改 card 行
/// （明令禁止）或（b）读路径 `retire`（`retirement` 账是 user/operator 的裁定，读路径写它
/// 就变成"消费一次召回 = 一次退场裁定"，且要镜像水位/备份）。闸格是读取侧唯一既不改行、
/// 又能让"这张卡已失效"立刻在注入面生效的落点。
#[test]
fn stale_反馈进_stale_判定且不再出声() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let (a, _b) = seed_pair(&m);
    record_reuse(&m, &a.id, "stale", Some("事实已变更"), NOW, "agent-reuse-rank").unwrap();
    // 检索面照旧返回它（过滤在闸门不在检索，G6 判据③）。
    let s = by_key(&m, &a);
    assert!(s.reuse.stale, "stale 反馈必须装进信号");
    assert!(s.above_floor(), "stale 不是低分：分数口径一字不动");
    // 闸门：cell = Stale、form = Silent。
    let d = decide(&s, NOW);
    assert_eq!(d.cell, Some(GateCell::Stale), "读取侧 stale 判定没生效");
    assert_eq!(d.form, Form::Silent, "stale 的卡不许出声");
    // 注入面（hook probe）：候选池里还有两条，但出声的只有没报 stale 的那张。
    let view = probe_view(&m, None, QUERY, NOW).unwrap();
    assert_eq!(view.candidates, 2, "候选池不因 stale 反馈缩小");
    assert_eq!(view.silent.counts().iter().map(|(c, n)| (*c, *n)).find(|(c, _)| *c == GateCell::Stale), Some((GateCell::Stale, 1)));
    assert_eq!(view.forms.len(), 1, "出声面只剩一张卡：{:?}", view.forms.iter().map(|d| d.key.key_string()).collect::<Vec<_>>());
    assert_ne!(view.forms[0].key, a, "stale 卡不许进出声面");
}

/// 缺表（刀 A 之前的只读旧副本）⇒ 零信号，读取面照旧能跑。
#[test]
fn 缺复用表等于无信号() {
    let dir = tempfile::tempdir().unwrap();
    let m = memory_handle(dir.path());
    let (a, _b) = seed_pair(&m);
    let before = order(&m);
    m.conn().execute_batch("DROP TABLE card_reuse_event").unwrap();
    let after = order(&m);
    assert_eq!(before, after, "缺表时复用信号必须整体缺席");
    assert_eq!(by_key(&m, &a).reuse, rc_search::ReuseSignal::default());
}
