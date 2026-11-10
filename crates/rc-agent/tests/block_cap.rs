//! 渲染器与阶梯账目的两条守卫（任务 6 记账的那条债在这一档兑现）：
//! ① `Decision::lines()` 说 full 占两个物理行，渲染器就必须画两个物理行——
//!    画成三个的话，`fit` 的上限与真实 stdout 就脱钩了（C2 的"硬上限"变成一句换算出来的话）；
//! ② `MAX_FORM_LINES` 这个上限**有两个执行处**（判定层的 `fit` 与载体 `InjectionBlock::from_lines`
//!    的那次 `take`）。用例 `上限有两个执行处_少一个就红` 逐处喂一条路：`fit` 那一支走判定层，
//!    另一支走**未经 `fit`** 的 forms，所以 `take` 一旦被删就当场红（等式那一支测不到它）。

use rc_agent::surface::probe_block;
use rc_agent::FENCE;
use rc_memory::keys::{MemKey, MemTable};
use rc_memory::reader::RowFacts;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Verdict};
use rc_search::gate::{fit, Cells, Decision, Form, GateCell, ProbeView, MAX_FORM_LINES};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
/// 本文件自带的一串标记正文（`body_canary.rs` 那根金丝雀只住在那一个文件里）。
const MARK: &str = "阶梯夹具正文绝不出现在第二行-7be1";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() }
}

/// 一条 full 档的判定，正文带着 `MARK`。不经检索：这是渲染器的输入形状，不是闸门的。
fn full_decision(i: usize) -> Decision {
    let mut id = format!("{i:x}");
    while id.len() < 64 {
        id.push('0');
    }
    let facts = RowFacts {
        key: MemKey { table: MemTable::Card, id: id.clone() },
        created_at_ms: NOW,
        kind: "decision".into(),
        title: format!("阶梯夹具标题{i}"),
        body: format!("{MARK} 第{i}句。后面的句子不该被画出来"),
        as_of_ms: NOW,
        valid_to_ms: None,
        superseded_by: None,
        retired: false,
        rejected: false,
        verdict: Verdict::Passed,
        attestation: None,
        anchor_role: AnchorRole::Human,
        cooldown_until_ms: None,
        // 任务 10 加的三字段：这个夹具喂注入块渲染，本任务的过滤判据不读它们，
        // 但**必须写合成署名那两串**（与 `BatchCtx::synthetic()` 逐字同串）——空串会让
        // "运维面上出现空 device_id"这一形状在测试里既造不出也测不到。
        scope: scope(),
        device_id: "dev-synthetic-1".into(),
        source_agent: "synthetic-agent".into(),
    };
    Decision { key: facts.key.clone(), form: Form::Full, cell: None, score: i64::MAX, facts }
}

#[test]
fn 五行_full_的注入块只画到上限() {
    // 五个 `Decision`（每个两行 = 十行）直接进渲染器，`fit` 不在之路上。
    let view = ProbeView {
        forms: (0..5).map(full_decision).collect(),
        silent: Cells::default(),
        candidates: 5,
    };
    let block = rc_agent::surface::probe_block(&view);
    let n = block.text().lines().count();
    assert_eq!(n, MAX_FORM_LINES, "注入块画了 {n} 个物理行，上限是 {MAX_FORM_LINES} ⇒ 截断没在类型上执行");
    assert!(block.text().lines().all(|l| l.starts_with(rc_agent::FENCE)), "围栏不是每行前缀（V3）");
}

#[test]
fn full_档在活体检索下画两个物理行且与_lines_账目相同() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let k = put_card(
        root.memory(),
        &FactContent {
            kind: "decision".into(),
            title: "阶梯夹具的一句决定".into(),
            body: format!("{MARK}第一句。第二句不许被画出来"),
            scope: scope(),
            valid_to_ms: None,
        },
        &[Provenance { turn_uid: "b".repeat(64), anchor_role: AnchorRole::Human }],
        NOW,
    )
    .unwrap();
    // V5：full 只由可核查证据打开——这里走 `verdict = passed` 那一臂。
    rc_memory::set_verdict(root.memory(), &k, Verdict::Passed, NOW).unwrap();
    rc_index::rebuild(root.memory(), NOW).unwrap();

    let view = rc_agent::surface::probe_via_search(root.memory(), None, "阶梯夹具的一句决定", NOW).unwrap();
    assert_eq!(view.forms.len(), 1, "夹具不止一行，下面那个等式就成了空的");
    assert_eq!(view.forms[0].form, Form::Full, "verdict=passed 没打开 full 档");
    let block = rc_agent::surface::probe_block(&view);
    let text = block.text();
    let lines: Vec<&str> = text.lines().collect();
    let accounted: usize = view.forms.iter().map(|d| d.lines()).sum();
    assert_eq!(lines.len(), accounted, "渲染器的物理行数与 `Decision::lines()` 的账不符：fit 的上限就此与真实 stdout 脱钩");
    assert_eq!(lines.len(), 2, "full 档不是两个物理行");
    assert!(lines.iter().all(|l| l.starts_with(rc_agent::FENCE)), "围栏不是每行前缀（V3）");
    let body_lines = lines.iter().filter(|l| l.contains(MARK)).count();
    assert_eq!(body_lines, 1, "正文被画到了不止一个物理行");
}

/// 渲染素材从真库里取，不手编正文：`first_sentence` 的中文边界只在真正文上测得出。
fn base_facts(dir: &std::path::Path) -> RowFacts {
    let root = Root::open(dir).unwrap();
    let k = rc_memory::synth::seed(root.memory(), NOW).unwrap().remove(0);
    rc_memory::reader::lookup(root.memory(), &k).unwrap().unwrap()
}

/// 手工拼 `ProbeView`，`forms` 是**未经 `fit` 的**。
/// 用 `probe_view` 拿到的东西测渲染层的剪刀永远绿——它在进渲染层之前就被截好了。
/// 偏离 brief 的一处形态（语义逐位相同）：原文 `ProbeView { forms, …, candidates: forms.len() }`
/// 编译不过——结构体字面量按书写序求值，`forms` 在第 1 个字段就被移走，第 3 个字段再
/// `forms.len()` 是 E0382（实测，见报告）。先取长度再构造，`candidates` 的值与原文同一个算法。
fn raw_view(forms: Vec<Decision>) -> ProbeView {
    let candidates = forms.len();
    ProbeView { forms, silent: Cells::default(), candidates }
}

fn d(form: Form, facts: RowFacts) -> Decision {
    Decision {
        key: facts.key.clone(),
        form,
        cell: if form == Form::Silent { Some(GateCell::LowScore) } else { None },
        score: 1024,
        facts,
    }
}

#[test]
fn 渲染行数等于各档行数之和_这条等式是上限所在层的唯一答案() {
    // 断**等式**而不是断数字：数字改了（比如 full 占 3 行）这条仍绿，
    // 而"渲染层与判定层各算一次行"这个错误形状一定红。
    let dir = tempfile::tempdir().unwrap();
    let f = base_facts(dir.path());
    let mixed = vec![
        d(Form::Full, f.clone()),
        d(Form::Signpost, f.clone()),
        d(Form::Silent, f.clone()),
        d(Form::Signpost, f.clone()),
    ];
    let fitted = fit(&mixed);
    let text = probe_block(&raw_view(fitted.clone())).text();
    assert_eq!(
        text.lines().count(),
        fitted.iter().map(|x| x.lines()).sum::<usize>(),
        "渲染层与判定层各算一次行"
    );
    // 三条臂各自点名（合起来的和正好是 MAX_FORM_LINES，所以这也是"上限被用满"的一次实测）
    assert_eq!(probe_block(&raw_view(vec![d(Form::Signpost, f.clone())])).text().lines().count(), 1);
    assert_eq!(probe_block(&raw_view(vec![d(Form::Full, f.clone())])).text().lines().count(), 2);
    assert!(probe_block(&raw_view(vec![d(Form::Silent, f.clone())])).is_empty(), "沉默不该出一行");
    // 沉默的表示就是它（V3）：空块 ⇒ 空串 ⇒ `stdout.is_empty()`
    assert_eq!(probe_block(&raw_view(vec![])).text(), "");
}

#[test]
fn 上限有两个执行处_少一个就红() {
    let dir = tempfile::tempdir().unwrap();
    let f = base_facts(dir.path());
    // 执行处 ①：`fit`（判定层，按各档行数累加、溢出即停）
    let many = vec![d(Form::Full, f.clone()); MAX_FORM_LINES + 2];
    let fitted = fit(&many);
    assert!(fitted.iter().map(|x| x.lines()).sum::<usize>() <= MAX_FORM_LINES);
    // 执行处 ②：`InjectionBlock::from_lines` 的 `take`。
    // 这一支喂**未经 fit** 的 forms，所以 `take` 一旦被删就当场红（上面那条等式测不到它）。
    let text = probe_block(&raw_view(many)).text();
    assert!(text.lines().count() <= MAX_FORM_LINES, "越界：{} 行", text.lines().count());
}

#[test]
fn 换行符不许把注入块撑过三行() {
    // (3) 那条洞的正身：物理行数由代码决定，不由数据决定。
    let dir = tempfile::tempdir().unwrap();
    let mut f = base_facts(dir.path());
    f.body = "第一行\n第二行\n第三行\n第四行\n第五行。尾".into();
    f.title = "标题\n也带换行".into();
    let text = probe_block(&raw_view(vec![d(Form::Full, f.clone()); 5])).text();
    assert!(text.lines().count() <= MAX_FORM_LINES, "越界 {} 行", text.lines().count());
    for line in text.lines() {
        assert!(line.starts_with(FENCE), "围栏不是每行前缀（V3）：{line:?}");
        assert!(!line.contains('\r'), "CR 漏进 stdout：LF 纪律");
    }
    // 标题那一支（signpost 直接插 `title` 原样，所以它也得测）
    let sp = probe_block(&raw_view(vec![d(Form::Signpost, f.clone())])).text();
    assert_eq!(sp.lines().count(), 1, "一个换行把一条提醒变成两条：{sp:?}");
    // 反向的一半：替换成空格，**不许删**。删 = 静默截断，读的人无从知道少了什么。
    assert!(sp.contains("标题") && sp.contains("也带换行"), "剪刀变成了截断：{sp:?}");
    // 加强 ①（证伪 M4 的产物）：上面那句对"把控制字符**删掉**"这一型变异没有牙——
    // 删除后 `标题` 与 `也带换行` 两个子串都在（"标题也带换行"），实测整条用例仍绿。
    // 换行必须**换成一个空格**而不是消失，只有这个形状能把它和删除区分开。
    assert!(sp.contains("标题 也带换行"), "控制字符被删掉了而不是换成空格：{sp:?}");
    let full = probe_block(&raw_view(vec![d(Form::Full, f)])).text();
    assert!(full.contains("第一行") && full.contains("第二行"), "剪刀变成了截断：{full:?}");
    // 同上，full 档那一支（加强 ① 的第二处）。
    assert!(full.contains("第一行 第二行"), "控制字符被删掉了而不是换成空格：{full:?}");
    // 加强 ②（证伪 M6 的产物）：CR 那一支原夹具**一个都没带**，于是上面 `!line.contains('\r')`
    // 那条逐字断言在任何变异下都不会红（实测：把 `one_line` 改成"只映射 LF"整条用例仍绿）。
    // 这里补一条真带 CR 的夹具，让 `is_control` 的 CR 分支被测得到——两个渲染档都过一遍。
    let mut cr = base_facts(dir.path());
    cr.body = "甲\r\n乙\r丙。丁".into();
    cr.title = "标题\r带回车".into();
    for view in [d(Form::Signpost, cr.clone()), d(Form::Full, cr)] {
        let t = probe_block(&raw_view(vec![view])).text();
        assert!(!t.contains('\r'), "CR 漏进 stdout（LF 纪律）：{t:?}");
        // 物理行数 ⇔ 围栏数：一条没被换成空格的换行会让某一行的开头没有 `[recollect] `（V3）。
        assert_eq!(t.lines().count(), t.matches(FENCE).count(), "物理行数与围栏数脱钩：{t:?}");
    }
}
