//! 沉默五格的**排他性与可逆性**（任务 6 · Step 3）。
//!
//! 这一档断的是 G6 的两条判据在运行时的形状：
//! ①一格一个原因（两行落进同一格 = 夹具失去排他性，格子读数就不再是"为什么没提它"）；
//! ②格子必须回指它的**载体列**（说它是 Stale，那 `valid_to_ms`/`superseded_by`/`retirement` 必须真的在，
//!   不许靠标题里的字猜——把过滤挪进检索层的那一版实现，第一个红就该红在这里）；
//! ③沉默不许把行从库里抹掉（低分行仍可 `lookup`，退场行 resolve 后原句柄回来，守卫 13）。
//!
//! 夹具全部合成（守卫 12）：编造的中文短句，无 CANARY、无路径、无密钥。

use rc_memory::keys::MemKey;
use rc_memory::{
    lookup, put_card, set_verdict, AnchorRole, FactContent, Provenance, Scope, ScopeKind, Verdict,
};
use rc_search::gate::{decide, fit, probe_view, Cells, Form, GateCell, MAX_FORM_LINES};
use rc_search::rank::rank;
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
const Q: &str = "格子测试合成词";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() }
}
fn human_prov(uid: &str) -> Vec<Provenance> {
    vec![Provenance { turn_uid: uid.repeat(64).chars().take(64).collect::<String>(), anchor_role: AnchorRole::Human }]
}

#[test]
fn 五格各一条_每条沉默恰好一个原因() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    rc_memory::synth::seed_cells(m, NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let ranked = rank(m, None, NOW, Q).unwrap();
    assert!(ranked.len() >= 5, "五格夹具没都进候选池：{}", ranked.len());
    let decisions: Vec<_> = ranked.iter().map(|s| decide(s, NOW)).collect();
    let cells = decisions.iter().filter(|d| d.form == Form::Silent);
    let mut seen: Vec<GateCell> = Vec::new();
    for d in cells {
        let c = d.cell.expect("form=Silent 而 cell=None ⇒ 造出了读不懂的沉默");
        assert!(!seen.contains(&c), "两行落进同一格 {c:?}，五格夹具的用例失去排他性");
        seen.push(c);
    }
    assert_eq!(seen.len(), 5, "沉默格子不足五种：{seen:?}");
    for c in GateCell::ALL {
        assert!(seen.contains(&c), "缺格 {c:?}");
    }
}

#[test]
fn 归格必须与载体列一致_不许靠标题猜() {
    // 这一条把"格子"从字符串升回它的数据来源：说它是 Stale，那 valid_to 必须真的过期；
    // 说它是 Cooldown，那 cooldown_until_ms 必须真的没到。实现者把过滤挪到检索层时，这里先红。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    rc_memory::synth::seed_cells(m, NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    for s in rank(m, None, NOW, Q).unwrap() {
        let d = decide(&s, NOW);
        let Some(cell) = d.cell else { continue };
        match cell {
            GateCell::Stale => assert!(
                s.facts.retired || s.facts.superseded_by.is_some()
                    || s.facts.valid_to_ms.is_some_and(|v| v < NOW),
                "{} 报 Stale 但没有过期载体", d.key.key_string()
            ),
            GateCell::Cooldown => assert!(
                s.facts.cooldown_until_ms.is_some_and(|t| t > NOW),
                "{} 报 Cooldown 但冷却列没到", d.key.key_string()
            ),
            GateCell::Rejected => assert!(
                s.facts.rejected || s.facts.verdict == Verdict::Failed,
                "{} 报 Rejected 但两半都没证据", d.key.key_string()
            ),
            GateCell::WeakAnchor => assert_eq!(s.facts.anchor_role, AnchorRole::Agent, "{}", d.key.key_string()),
            GateCell::LowScore => assert_eq!(s.score, 0, "{} 报低分但分数不为 0", d.key.key_string()),
        }
    }
}

#[test]
fn 既过期又低分的行必须报过期不是低分() {
    // 优先级顺序的执法点（decide 里那五个 if 的顺序）。反序不报错，只是把"人判过的失效"
    // 伪装成"算法嫌它短"，而这两者在 export 里的含义完全相反。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let long: String = (0..3000).map(|i| format!("格子测试合成词 记录{i} ")).collect();
    let c = FactContent {
        kind: "situation".into(),
        title: "过期且超长卡".into(),
        body: long,
        scope: scope(),
        valid_to_ms: Some(NOW - 1),
    };
    let k = put_card(m, &c, &human_prov("x"), NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let s = rank(m, None, NOW, Q).unwrap().into_iter().find(|x| x.key == k).unwrap();
    assert_eq!(s.score, 0, "先确认它确实也满足低分");
    assert_eq!(decide(&s, NOW).cell, Some(GateCell::Stale), "过期被低分掩盖");
}

#[test]
fn 低分沉默的行仍在_recall_面上可查() {
    // G6 判据③的前半（后半在任务 7 的工具面上断，两半不能只断一侧）。
    // 只断"它不在 probe 输出"的话，实现者会把过滤挪进检索层，症状变成"记忆丢了"。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    // 偏离 brief（沿用前半的偏离 ③ 同一口径）：原文的 3000 落在 `score == 1 == FLOOR`
    // （实测 `matched=5, row_len=3004 ⇒ 1024*5/3004 == 1`），`above_floor` 仍为真 ⇒ 这一行根本
    // 不进 LowScore 格，`counts()[4].1 == 1` 必红。0 分的条件是 `row_len > 1024·matched`，
    // 所以垫到 6000（`tests/gate_switches.rs:245` 同一个数、同一个理由）。
    let k = rc_memory::synth::seed_padded(m, NOW, 6000).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let pool = rank(m, None, NOW, "长行低分卡 记录").unwrap();
    let view = probe_view(m, None, "长行低分卡 记录", NOW).unwrap();
    assert!(view.forms.iter().all(|d| d.key != k), "低分行被注入了");
    assert!(lookup(m, &k).unwrap().is_some(), "低分行在 recall 面上查不到 ⇒ 过滤挪进了检索层");
    assert_eq!(view.silent.counts()[4].1, 1, "低分那一格没计数（ALL 声明序的第 5 项是 LowScore）");
    // 偏离 brief（按本档 Step 5 第 7 条自己给出的口径补上）：`candidates` 必须等于**候选池**大小，
    // 而不是注入条数。实测：这一条的池子只有 1 行，"先截断到 3 条再判"在那里截不出差别（无牙），
    // 所以证伪 7 的真正载体是下面 `注入行数上限不随库变大` 里同形状的那句断言（池子 200+ 行）。
    // 这句留着不为证伪服务，它钉的是"candidates 的口径 = 池子，不是注入条数"这个定义本身。
    assert_eq!(view.candidates, pool.len(), "候选池计数被截断了 ⇒ 过滤挪进了检索层");
}

#[test]
fn 退场行不在候选池_resolve_后原句柄回来() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let k = put_card(m,
        &FactContent { kind: "decision".into(), title: "退场与回归卡 格子测试合成词".into(),
            body: "格子测试合成词 一句够短的话".into(), scope: scope(), valid_to_ms: None },
        &human_prov("h"), NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    assert!(probe_view(m, None, Q, NOW).unwrap().forms.iter().any(|d| d.key == k));
    rc_memory::retire(m, &k, "合成理由", NOW).unwrap();
    let after = probe_view(m, None, Q, NOW).unwrap();
    assert!(!after.forms.iter().any(|d| d.key == k));
    assert_eq!(after.silent.counts()[1].1, 1, "退场必须落 Stale 格（ALL[1]）");
    rc_memory::resolve_retirement(m, &k, NOW + 1).unwrap();
    let back = probe_view(m, None, Q, NOW + 1).unwrap();
    assert!(back.forms.iter().any(|d| d.key == k), "resolve 后句柄没回来");
}

#[test]
fn 注入行数上限不随库变大() {
    // G6 判据①。200 条同匹配行足够把这条判据和"刚好只有三行"分开；
    // "一千条库"那一半在任务 11 的脚本里用一次性造的库跑，不进单测时间预算（那是 fsync 不是判据）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    rc_memory::synth::seed(m, NOW).unwrap();
    // 偏离 brief：原文第一次 `probe_view` 前没有 `rebuild`，而 `rc-index` 在未建索引时响亮报错 ⇒
    // 那一句直接返回 `Err(Index(NotBuilt))`，brief 这条用例编译得过但**当场 panic**
    // （实测：`causes.rs` 里 `called \`Result::unwrap()\` on an \`Err\` value: Index(NotBuilt)`）。
    // 补上 rebuild：上限不随库变大这句话要有两次**都非零**的读数才成立。
    rc_index::rebuild(m, NOW).unwrap();
    let small = probe_view(m, None, "同分合成词甲", NOW).unwrap();
    let small_lines: usize = small.forms.iter().map(|d| d.lines()).sum();
    for i in 0..200 {
        rc_memory::synth::seed_variants(m, NOW, i).unwrap();
    }
    rc_index::rebuild(m, NOW).unwrap();
    let big = probe_view(m, None, "同分合成词甲", NOW).unwrap();
    assert!(big.candidates > small.candidates + 100, "库没长大：{} → {}", small.candidates, big.candidates);
    let big_lines: usize = big.forms.iter().map(|d| d.lines()).sum();
    assert!(big_lines <= MAX_FORM_LINES, "长大到 {big_lines} 行，超上限");
    assert!(big_lines > 0, "两次读数都是 0 行的话，下面那条相等断言什么也没断（加强，非放宽）");
    assert_eq!(small_lines, big_lines, "行数随库变大：{} → {}", small_lines, big_lines);
    // 偏离 brief（搬载体，配合 Step 5 第 7 条）：候选池计数必须等于**池子大小**而不是注入条数。
    // brief 把这句只挂在 `低分沉默的行仍在_recall_面上可查` 上，而那条用例的池子只有 1 行，
    // "先截断到 3 条再判"在那里根本截不出差别（实测无牙）；这条的池子是 200+ 行，才是它的载体。
    assert_eq!(big.candidates, rank(m, None, NOW, "同分合成词甲").unwrap().len(),
        "候选池计数不等于池子大小 ⇒ 有人把过滤/截断挪到了判定之前");
}

#[test]
fn fit_溢出即停不跳着塞() {
    // 这条断的是 `break` 而不是 `continue`：两条 full（2+2）里第二条放不下时，
    // 后面那条 1 行的 signpost 也不许塞进来——否则高分行被低分行换掉，相关性就没意义了。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    rc_memory::synth::seed(m, NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let q = "同分合成词甲";
    // 偏离 brief（搬载体）：原文的池子里**只有 signpost**（`seed` 不写 verdict、不写正面表态，
    // 一行 1 行），而 1 行的项在 used=3 之后无论 `break` 还是 `continue` 都塞不进去 ⇒
    // 把 `fit` 的 `break` 改成 `continue` 时这条用例照绿（Step 5 第 4 条无牙）。
    // 注释自己写的形状是"两条 full（2+2）里第二条放不下、后面那条 1 行的 signpost 也不许塞"，
    // 所以按那个形状造：把除最后一条以外的开口行升到 full（V5 的 `verdict=Passed` 臂），
    // 池子顺序是 `rank` 的全序，改 verdict 不动分数也不动句柄 ⇒ 顺序不变。
    let opened: Vec<MemKey> = rank(m, None, NOW, q).unwrap().iter()
        .map(|s| decide(s, NOW))
        .filter(|d| d.form != Form::Silent)
        .map(|d| d.key)
        .collect();
    assert!(opened.len() >= 3, "开口行不足三条，fit 没有溢出可断：{}", opened.len());
    for k in opened[..opened.len() - 1].iter() {
        set_verdict(m, k, Verdict::Passed, NOW).unwrap();
    }
    let all: Vec<_> = rank(m, None, NOW, q).unwrap().into_iter().map(|s| decide(&s, NOW)).collect();
    let forms: Vec<Form> = all.iter().filter(|d| d.form != Form::Silent).map(|d| d.form).collect();
    assert!(forms.len() >= 3, "改档位后开口行反而少了：{forms:?}");
    assert_eq!(forms[0], Form::Full, "夹具首行必须是 full，否则溢出没有载体：{forms:?}");
    assert_eq!(forms[1], Form::Full, "夹具第二行必须也是 full（2+2 才溢出）：{forms:?}");
    assert_eq!(*forms.last().unwrap(), Form::Signpost, "末行必须是 1 行的 signpost：{forms:?}");
    let fitted = fit(&all);
    assert!(fitted.iter().map(|d| d.lines()).sum::<usize>() <= MAX_FORM_LINES);
    // 前缀性质：fitted 必须是 all 里非沉默项的一个**前缀**（顺序子集，不许有洞）。
    let opened: Vec<String> = all.iter().filter(|d| d.form != Form::Silent).map(|d| d.key.key_string()).collect();
    let got: Vec<String> = fitted.iter().map(|d| d.key.key_string()).collect();
    assert_eq!(got, opened[..got.len()], "fit 不是前缀 ⇒ 有人把 break 写成了 continue");
}

#[test]
fn counts_是唯一的计数出口且五格补齐到零() {
    let mut c = Cells::default();
    c.record(GateCell::LowScore);
    let got = c.counts();
    assert_eq!(got.len(), 5);
    assert_eq!(got.iter().map(|(k, v)| (k.slug(), *v)).collect::<Vec<_>>(),
        vec![("rejected".to_string(), 0), ("stale".to_string(), 0), ("cooldown".to_string(), 0),
             ("weak_anchor".to_string(), 0), ("low_score".to_string(), 1)]);
    assert_eq!(c.total(), 1);
}

#[test]
fn 引用次数不许参与档位判定() {
    // D10 反面：把被引用/被出处次数偷偷变成 importance，会同时破 GOALS §3 两条。
    // 同一行在 0 条与 50 条额外出处下，form 与 cell 必须逐位相同，只许候选顺序不动（同一行）。
    //
    // **本档的边界（评审 M-2 写在这里，不再只躺在报告里）**：用例名说的是"引用次数"，
    // 载体却只有 `provenance` 的**条数**——这不是漏测，是当期无牙可长：
    // `crates/rc-store/src/ddl.rs:77-100` 的三张候选表没有任何 per-memory 的计数列，
    // 而 `crates/rc-memory/tests/ownership.rs:61` 把 `ref_count` / `importance` 这一族列名
    // 直接判了死刑（守卫 7 + GOALS §3）。要造那种载体只能加列（破上面这两条）
    // 或者让闸门现查会话库（破 V6："闸门只读 `RowFacts`"）。
    // 所以"被**引用**次数"这一半的归宿是 `vote` 账落地的那个阶段：等那张账有了 per-memory 计数，
    // 这一条必须在它上面重开一遍（同一行、计数变多、form/cell/score 逐位相同）。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let c = FactContent { kind: "decision".into(), title: "引用计数卡 格子测试合成词".into(),
        body: "格子测试合成词 一句够短的话".into(), scope: scope(), valid_to_ms: None };
    let k = put_card(m, &c, &human_prov("h"), NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let before = decide(&rank(m, None, NOW, Q).unwrap().into_iter().find(|x| x.key == k).unwrap(), NOW);
    assert_eq!(rc_memory::provenance_of(m, &k).unwrap().len(), 1, "起点不是 1 条出处");
    // 偏离 brief（换载体，语义不变）：原文用 `m.conn().execute(… rusqlite::params![…])` 裸插 50 行
    // `provenance`。依裁定 R28，`rc-search` 不许新增 `rusqlite` 依赖（dev 依赖也算），
    // 而且裸 SQL 正是 `synth.rs` 开头声明的"绕过守卫 11 的旁路"形状。
    // 这里走公开写入口：id 是内容的函数（`identity.rs`，时钟与出处都不进 preimage），
    // 所以同一份内容再 put 一次 = 行不动（`ON CONFLICT(id) DO NOTHING`）+ 多出 50 条出处
    // （`writers_x.rs:107-110` 的 INSERT OR IGNORE），比裸插更贴"被引用次数变多"的原意。
    let more: Vec<Provenance> = (0..50)
        .map(|i| Provenance { turn_uid: format!("{:0>2}{}", i, "b".repeat(62)), anchor_role: AnchorRole::Human })
        .collect();
    put_card(m, &c, &more, NOW).unwrap();
    assert_eq!(rc_memory::provenance_of(m, &k).unwrap().len(), 51,
        "出处没变多 ⇒ 这条用例什么都没断（载体没落地）");
    rc_index::rebuild(m, NOW).unwrap();
    let after = decide(&rank(m, None, NOW, Q).unwrap().into_iter().find(|x| x.key == k).unwrap(), NOW);
    assert_eq!(before.form, after.form, "出处次数改了形态");
    assert_eq!(before.cell, after.cell);
    assert_eq!(before.score, after.score, "出处次数改了分数");
}

#[test]
#[ignore = "G6① 的一千行档：1000 次播种 = 1000 次提交，本机数十秒。它不进全量预算（那是把判据降级成速度），\
             由 scripts/check-phase1.sh 显式 --ignored 跑一次。红法有两种，都要读得出来：\
             `candidates` 不到 900 ⇒ 库没长大（判据没跑到）；行数不等 ⇒ 上限真的随库变大了（判据破了）。"]
fn 注入行数上限在一千行库上仍与空库相同() {
    // G6 判据①的大档版：200 行那条把"上限"和"刚好只有三行"分开，这一条把"上限"和
    // "库大到某个程度就有人往库里塞第二条"分开。GOALS G6 写的是"一千条"，这里就一千条。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    rc_memory::synth::seed(m, NOW).unwrap();
    rc_index::rebuild(m, NOW).unwrap();
    let small = probe_view(m, None, "同分合成词甲", NOW).unwrap();
    let small_lines: usize = small.forms.iter().map(|d| d.lines()).sum();

    for i in 0..1000 {
        rc_memory::synth::seed_variants(m, NOW, i).unwrap();
    }
    rc_index::rebuild(m, NOW).unwrap();

    let big = probe_view(m, None, "同分合成词甲", NOW).unwrap();
    assert!(big.candidates >= 900, "库没长大到一千行档：候选 {} 条", big.candidates);
    let big_lines: usize = big.forms.iter().map(|d| d.lines()).sum();
    assert!(big_lines <= MAX_FORM_LINES, "长大到 {big_lines} 行，超上限");
    assert_eq!(small_lines, big_lines, "行数随库变大：{} → {}", small_lines, big_lines);
}
