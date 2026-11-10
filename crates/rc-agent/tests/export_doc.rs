//! `export`（C3）的两条读数：超限那句"还有 N 行未列出"，以及每一行为什么沉默。
//! 本文件的第一条用例是那句"未列出"在**整个阶段 1 里唯一一次被执行**（生产路径上
//! `ExportRequest::all()` 固定 `limit: None`），所以 Step 7 第 6 项证伪专门改坏它。

use rc_memory::{keys::MemKey, synth};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;

fn seeded(dir: &std::path::Path, extra: usize) -> Vec<MemKey> {
    let root = Root::open(dir).unwrap();
    let mut keys = synth::seed(root.memory(), NOW).unwrap();
    for i in 0..extra {
        keys.extend(synth::seed_variants(root.memory(), NOW, i).unwrap());
    }
    // 导出件不读索引（`reader::facts` 那条路），这里建索引是为了让同一份库也能被检索面复用；
    // 建与不建都不该改变导出件的一个字，所以这条用例不拿它做判据。
    rc_index::rebuild(root.memory(), NOW).unwrap();
    keys
}

#[test]
// 裁定 R38：用例名逐字取 `fn 还有N行未列出这一句必须真的被执行` 的字面（Step 7 的 export_doc 段 + Step 8 判据 8g），任务 11 的 `one --exact` 点它的名，
// 所以这里必须显式放行 `non_snake_case`——这个名字是**验收契约的一部分**，不是随手写的标识符。
#[allow(non_snake_case)]
fn 还有N行未列出这一句必须真的被执行() {
    let dir = tempfile::tempdir().unwrap();
    let keys = seeded(dir.path(), 18);
    assert_eq!(keys.len(), 25, "夹具行数错了：seed 七行 + 变体 18 行");
    let root = Root::open(dir.path()).unwrap();
    let doc = rc_agent::export::render_export(
        root.memory(),
        // S1b-C4b1：`ExportRequest` 加了 `viewer`。这条用例断的是 `limit` 那两句读数，
        // 读者身份不在其列 ⇒ 显式 `None`（不过闸，与 `ExportRequest::all` 同口径）。
        &rc_agent::export::ExportRequest { now_ms: NOW, limit: Some(10), viewer: None },
    )
    .unwrap();
    let text = doc.text();
    assert!(text.contains("还有 15 行未列出"),"超限那句没出现：{text}");
    // 同一份账的另一半：列出的行数与"未列出"的条数必须加起来等于总数（两个真相的判据）。
    assert!(text.contains("10 rows listed of 25."), "列出计数与未列出读数不自洽：{text}");
    // 第二个数据点：换一个 limit 再导一次，那句里的数字必须跟着走。
    // 只有这一个期望值的话，生产码里写死 `15` 也能绿（Step 7 第 6 项第一次跑就是这么发现的），
    // 所以这条用例的职责是"那句话是**算出来的**"，不是一个字符串匹配。
    let doc2 = rc_agent::export::render_export(
        root.memory(),
        &rc_agent::export::ExportRequest { now_ms: NOW, limit: Some(7), viewer: None },
    )
    .unwrap();
    let text2 = doc2.text();
    assert!(text2.contains("还有 18 行未列出"), "换了 limit 却没换读数 ⇒ 那句是写死的：{text2}");
    assert!(text2.contains("7 rows listed of 25."), "列出计数与未列出读数不自洽：{text2}");
}

#[test]
fn 全量导出里没有未列出这一句() {
    let dir = tempfile::tempdir().unwrap();
    seeded(dir.path(), 18);
    let root = Root::open(dir.path()).unwrap();
    let doc = rc_agent::export::render_export(root.memory(), &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap();
    let text = doc.text();
    assert!(!text.contains("未列出"), "limit=None 却报未列出：那句话就成了装饰");
    assert!(text.contains("25 rows listed of 25."), "{text}");
}

#[test]
fn 沉默原因与格子计数在导出件里与判定同源() {
    // 五格夹具：每行只带它那一格的证据（`synth::seed_cells` 的纪律）。
    // `low_score` 那一格在这里**数不到**，是刻意的形状而不是漏断：没有查询就没有分数，
    // `export`/`recall` 面用 `i64::MAX` 显式表达"分数这一开关不参与"（见那两处注释），
    // 所以超长行在导出件里是开口的 —— 这里就把"它没被算进低分"也断住。
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    rc_memory::synth::seed_cells(root.memory(), NOW).unwrap();
    let doc = rc_agent::export::render_export(root.memory(), &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap();
    let text = doc.text();
    for slug in ["rejected", "stale", "cooldown", "weak_anchor"] {
        assert!(text.contains(&format!("reason {slug}")), "导出件没写这一行为什么沉默：{slug} ⇒ {text}");
        assert!(text.contains(&format!("- {slug}: 1")), "沉默格子计数错了：{slug} ⇒ {text}");
    }
    assert!(text.contains("- low_score: 0"), "export 面不该数到低分：{text}");
    // 沉默的行也进导出件（C3 与 G6③的合取）：五行都在，一行不少。
    let listed = text.lines().filter(|l| l.starts_with("- `")).count();
    assert_eq!(listed, 5, "沉默的行被从导出件里滤掉了：{text}");
}

/// S5/G1 的第二半：**沉默格子之和必须与"没有沉默原因的行"对得上**。上面那条用例逐 slug
/// 断言"这一格为 1"，却没有一句把两本账钉在一起 ⇒ 某一格被漏记、或 `shown` 的行被错记进
/// 某个格子，它仍然绿（它只数了四格，第五格恰好是 `low_score: 0`）。
///
/// 夹具用最小的两行（1 条被否 + 1 条正常），断言三件事：`silence cells` 里唯一非零的格子是
/// `rejected: 1`；`reason shown` 恰一行；四格之和 + `shown` 行数 == `2 rows listed of 2.`。
/// **不改导出格式**：只读既有那一段，形状一个字节都没动。
#[test]
fn 沉默格子计数与列出的行数对得上() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "p-silence-pair".into() };
    let human = vec![Provenance { turn_uid: "c".repeat(64), anchor_role: AnchorRole::Human }];
    let rejected = put_card(m, &FactContent {
        kind: "decision".into(), title: "被否的那一条".into(),
        body: "这一条在导出件里必须写明它是被否的。".into(),
        scope: scope.clone(), valid_to_ms: None,
    }, &human, NOW).unwrap();
    rc_memory::attest(m, &rejected, rc_memory::Stance::Rejected, None,
                      Some("合成：与最新结论冲突"), NOW).unwrap();
    put_card(m, &FactContent {
        kind: "decision".into(), title: "正常的那一条".into(),
        body: "这一条没有任何沉默原因。".into(),
        scope, valid_to_ms: None,
    }, &human, NOW).unwrap();

    let text = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap().text();
    // 两行都进导出件；各自的原因写在各自那一行上（`shown` 不是一格，是"没有格"的渲染）。
    let listed = text.lines().filter(|l| l.starts_with("- `")).count();
    assert_eq!(listed, 2, "两行都必须列出（沉默的行也进导出件）：{text}");
    // 行形状是 `… (as_of … , form … , reason <slug>)`：从 `reason ` 起数到右括号为止。
    let shown = text.matches("reason shown)").count();
    assert_eq!(text.matches("reason rejected)").count(), 1, "被否那一行没写原因：{text}");
    assert_eq!(shown, 1, "正常那一行被错记了沉默原因：{text}");
    // `## silence cells` 那一段：五格必须齐（缺格补 0），且四格之和就是"没有开口的行"数。
    let cells: Vec<(String, usize)> = text.lines()
        .skip_while(|l| *l != "## silence cells").skip(1)
        .take_while(|l| l.starts_with("- "))
        .map(|l| {
            let (slug, n) = l.trim_start_matches("- ").split_once(": ").unwrap();
            (slug.to_owned(), n.parse().unwrap())
        })
        .collect();
    assert_eq!(cells.len(), 5, "沉默格子少了（缺格补 0 是那一处的纪律）：{text}");
    let sum: usize = cells.iter().map(|(_, n)| n).sum();
    assert_eq!(sum + shown, listed, "沉默格子之和 + shown 行数 != 列出的行数：{cells:?} ⇒ {text}");
    let rejected_cell = cells.iter().find(|(slug, _)| slug == "rejected").map(|(_, n)| *n);
    assert_eq!(rejected_cell, Some(1), "被否的那一行没记进 rejected 格：{cells:?}");
    assert!(cells.iter().all(|(slug, n)| slug == "rejected" || *n == 0),
            "别的格子被错记了：{cells:?}");
    // 同一份账的另一半：列出计数与总行数一致（两行都给了）。
    assert!(text.contains("2 rows listed of 2."), "列出计数与总行数不自洽：{text}");
}

use rc_memory::keys::{AnchorRole, Scope, ScopeKind};
use rc_memory::writers_x::{put_card, put_chains, put_event, put_timeline_rows, ChainContent,
                            FactContent, TimelineContent};
use rc_memory::Provenance;

#[test]
fn 导出件里两跳一跳与missing三种读数各在其位() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "p-synthetic".into() };
    let turn = |c: char| c.to_string().repeat(64);
    let human = |c: char| vec![Provenance { turn_uid: turn(c), anchor_role: AnchorRole::Human }];
    // 事实行一（下面当"一跳"的标本，也当"两跳"的依据行）
    let c1 = put_card(m, &FactContent { kind: "decision".into(), title: "那条决定".into(),
                                        body: "任何合并之前先跑全量测试。".into(),
                                        scope: scope.clone(), valid_to_ms: None },
                      &human('c'), NOW).unwrap().id;
    // 事实行二（写完就被删掉 ⇒ 它的引用行读出 missing）
    let c2 = put_card(m, &FactContent { kind: "decision".into(), title: "那条旧决定".into(),
                                        body: "那条旧决定后来没有落。".into(),
                                        scope: scope.clone(), valid_to_ms: None },
                      &human('d'), NOW).unwrap().id;
    // 两条推导时间线：各自锚在上面那张卡（`put_timeline_rows` 落 event 行 +
    // 一行 `ref_kind='card'` 的出处行——被导出列出的推导行只有这一族，边进不了 `facts`）
    for anchor in [c1.clone(), c2.clone()] {
        put_timeline_rows(m, &[TimelineContent {
            title: "那条时间线".into(),
            body: format!("那条决定排在发布前面。锚={anchor}"),
            scope: scope.clone(), time_anchor_ms: Some(NOW + 86_400_000),
            time_anchor_id: Some(anchor), time_confidence: "anchored".into(),
            basis_ids: vec![],
        }], &rc_memory::BatchCtx::synthetic(), NOW).unwrap();
    }
    // **missing 这一格怎么造的**：把依据行本身删掉。为什么不能"天生就没有"——
    // `put_timeline_rows` 落库前先过 `table_of`（依据 id 不在两张表 `anchor`/`card` 里 ⇒
    // `MemError::Sql`），所以"依据行不存在"这个状态**没有生产写入口**，它是防御性读数：
    // `provenance` 没有指向这两张表的外键（T1 的 DDL），而全树没有任何删事实行的语句，今天能造出它的只有
    // 库外的操作（阶段 4 的容量裁剪、跨设备合并、G4 配方少删一支）。这与 T6 的
    // `TargetStatus::Missing` 是同一个下界、同一个口径，两边都不许静默。
    // 这一句 SQL 为什么不塞进 `rc_memory::synth`：那个模块的头纪律②明写"播种一律走
    // `writers_*` 的公开入口，不裸写 SQL"，这个状态造不进夹具工厂。而在这里它是合法的：
    // `conn()` 的 inherent 方法不需要 `rusqlite` 进依赖集（判据 `cargo tree -p rc-agent
    // -e normal --depth 1 | grep rusqlite` 仍然为空），本文件同族的既有用例早就在用
    // `execute_batch` 打 `PRAGMA wal_checkpoint`（同目录的 `body_canary.rs` 与
    // `errors_are_four.rs` 里早有同形状的用法）。
    // id 是 sha256 hex ⇒ 拼进 SQL 没有引号风险，也不需要绑参。
    m.conn().execute_batch(&format!("DELETE FROM card WHERE id='{c2}'")).unwrap();

    // 第四格（**E43 补**）：**依据行在场、但它自己没有轮次出处**。根因与 ③ 不同而渲染形状不同——
    // ③ 是"依据不在"，④ 是"依据在、却没人能说它是哪一轮抽出来的"。`SELECT EXISTS` 那一支
    // 就是为它存在的：按"第二跳查到了就算在"实现的话 ④ 会被错标成 missing，标 missing 就是谎报
    // （运维会读成"那条依据已经没了"，而它明明在库里）。**E43 实测这一格造得出**——
    // 一行 `DELETE FROM provenance WHERE ref_kind='turn' AND mem_table='card'` 就够，
    // 实施者报告里"这个方向树里造不出形状"的登记是错的，已作废。
    let c3 = put_card(m, &FactContent { kind: "decision".into(), title: "那条无轮次决定".into(),
                                        body: "这一行在场，但没人说得出它是哪一轮抽出来的。".into(),
                                        scope: scope.clone(), valid_to_ms: None },
                      &human('h'), NOW).unwrap().id;
    put_timeline_rows(m, &[TimelineContent {
        title: "那条无轮次时间线".into(),
        body: "锚在一条没有轮次出处的依据上。".into(),
        scope: scope.clone(), time_anchor_ms: Some(NOW + 86_400_000),
        time_anchor_id: Some(c3.clone()), time_confidence: "anchored".into(),
        basis_ids: vec![],
    }], &rc_memory::BatchCtx::synthetic(), NOW).unwrap();

    // 第五格（**阶段 3 收口批补**）：上面四格的出处全是 Human ⇒ "依据在场带 role"这一格其实
    // **没有区分度**。收口批实测两刀：把 `export.rs` 的 Basis 分支改渲 `agent` ⇒ ① 那一格红
    // （断言确实盯住了渲染出来的角色）；把同一处**硬编码成 `human`** ⇒ 本文件四条全绿
    // （断言分不出"跟着来源走"和"写死一个"）。补一张 **Agent 出处**的卡，让两种角色在同一份
    // 导出件里并存 ⇒ 硬编码任一边都红，这一格才有牙。
    let c4 = put_card(m, &FactContent { kind: "decision".into(), title: "那条机器决定".into(),
                                        body: "这一行是 agent 自己抽出来的。".into(),
                                        scope: scope.clone(), valid_to_ms: None },
                      &[Provenance { turn_uid: turn('a'), anchor_role: AnchorRole::Agent }],
                      NOW).unwrap().id;
    put_timeline_rows(m, &[TimelineContent {
        title: "那条机器时间线".into(),
        body: "锚在一条 agent 出处的依据上。".into(),
        scope: scope.clone(), time_anchor_ms: Some(NOW + 86_400_000),
        time_anchor_id: Some(c4.clone()), time_confidence: "anchored".into(),
        basis_ids: vec![],
    }], &rc_memory::BatchCtx::synthetic(), NOW).unwrap();

    // ①② 里带 `turn('c')` 的那两条为什么落在 DELETE **之前**渲出来的这一份上：下面那行 DELETE
    // 删的是 `mem_table='card'` 的 turn 出处，而 c1 自己就是一条 card ⇒ **它的轮次出处也被删掉**。
    // 放在 DELETE 之后读，那两条读到的就不是"两跳渲染在不在"，而是"DELETE 有没有误伤事实行"，
    // 红起来含混（同一句 panic 覆盖两种坏法）。所以两份文本并存、断言各归各位：
    // ①② 的依据轮次形状断在**删之前**那一份，④ 的形状断在**删之后**那一份（④ 就是删出来的）。
    let text_before = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap().text();
    let lines_before: Vec<&str> = text_before.lines().collect();
    m.conn().execute_batch("DELETE FROM provenance WHERE ref_kind='turn' AND mem_table='card'")
        .unwrap();

    let text = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap().text();
    let lines: Vec<&str> = text.lines().collect();

    // ① 两跳：推导行先渲依据句柄，再渲那一条的出处轮次（更深一级缩进）。
    assert!(lines.contains(&format!("    ← card:{c1} (human)").as_str()),
            "推导行的第一跳没渲成依据句柄：{text}");
    assert!(lines_before.contains(&format!("        ← turn {} (human)", turn('c')).as_str()),
            "推导行的第二跳没在场（两跳被省成了单跳）：{text_before}");
    // ② 一跳：事实行自己照旧 `← turn <uid> (<role>)`。整行相等才算——`starts_with` 会被
    //    ①的那条八字节缩进行命中，那样这一格就空过了。
    assert!(lines_before.contains(&format!("    ← turn {} (human)", turn('c')).as_str()),
            "事实行的一跳渲染漂了：{text_before}");
    // ③ missing：明写，且不渲第二跳（"没有'然后'就不能报"）。
    assert!(lines.contains(&format!("    ← card:{c2} (missing)").as_str()),
            "依据行没了却没人明写 missing：{text}");
    // 这一句是③的另一半，也是 `basis_exists` 这一格的牙：被删那条卡自己的出处行还在库里
    // （删行不删出处），所以只看"查得到出处"就渲第二跳的话，这里会渲出一句
    // `        ← turn <64 个 d> (human)`——指认一个已经不存在的锚。它必须不在。
    assert!(!text.contains(&format!("← turn {}", turn('d'))),
            "依据行已不在，第二跳却照渲 ⇒ 导出件指认了一个不存在的锚：{text}");
    // 第四格的断言：依据**在场** ⇒ 渲依据句柄且**带 role**（不带 `(missing)`，那是 ③ 的形状）；
    // 而它没有轮次出处 ⇒ **不**渲第二跳，也**不许**拿别的轮次顶上。
    assert!(lines.contains(&format!("    ← card:{c3} (human)").as_str()),
            "依据在场就该渲依据句柄并带 role，不能错标成 missing：{text}");
    assert!(!text.contains(&format!("← turn {}", turn('h'))),
            "依据在场但没有轮次出处，第二跳不许凭空造一个：{text}");
    // 第五格的断言：这一条的 role 来自 **Agent** 出处的依据行，而同一份导出件里 ①④ 两条是
    // `human`。两条必须各读各的 ⇒ 上面那两刀的任一种硬编码（写死 `human` 或写死 `agent`）都红。
    assert!(lines.contains(&format!("    ← card:{c4} (agent)").as_str()),
            "依据是 agent 出处时角色没跟着来源渲染：{text}");
}

/// R5（工单 §5-2 · 段 C-1 的导出面）：链头在 `export` 里渲出**有序环节**，且守卫 12 不外泄正文。
///
/// 形状 `    ⇒ link <表>:<句柄> (<角色>)`（第四形，见 export.rs 那里的注释）。既有三种行
/// （`    ← turn …` / `    ← <表>:<id> …` / `        ← turn …`）一字不改；这一支自成一形，
/// 用 `⇒` 与既有三种的 `←` 区分、用 `link` 一词点明它是环节不是依据。
///
/// 变异形（两条都要真造过一次并留读数）：
/// ① 把 export.rs 里"链头渲有序环节"那一整段删掉 ⇒ 两条 `⇒ link` 行都不出现 ⇒ R5 红；
/// ② 把被引行的正文拼进环节那一行（拿 `body` 拼进去）⇒ 环节整行不再是 `    ⇒ link card:<id> (human)`，
///    下面那个"逐 `⇒ link` 行不含正文"的循环也红 ⇒ R5 红。（报告里记 ② 是否连带打红既有金丝雀——
///    本仓既有的 export 用例夹具里没有因果链行，`⇒ link` 那一支只有本条走进，见报告 §4 的实测。）
#[test]
fn 导出件把因果链头的有序环节渲出来且守卫12不外泄正文() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "p-chain".into() };
    let turn = |c: char| c.to_string().repeat(64);
    let human = |c: char| vec![Provenance { turn_uid: turn(c), anchor_role: AnchorRole::Human }];
    // 两个环节各是一张真 card，正文带各自独有串，用来验守卫 12：环节那一行不许把被引行的正文带出来。
    let a_body = "甲这一行独有的正文-KE51A";
    let b_body = "乙这一行独有的正文-KE51B";
    let a = put_card(m, &FactContent { kind: "decision".into(), title: "环节甲".into(),
                        body: a_body.into(), scope: scope.clone(), valid_to_ms: None },
                     &human('a'), NOW).unwrap().id;
    let b = put_card(m, &FactContent { kind: "decision".into(), title: "环节乙".into(),
                        body: b_body.into(), scope: scope.clone(), valid_to_ms: None },
                     &human('b'), NOW).unwrap().id;
    let mech = "甲先于乙是这条链的机制句";
    put_chains(m, &[ChainContent { mechanism: mech.into(), link_ids: vec![a.clone(), b.clone()],
                                  basis_ids: vec![], causal_type: "causes".into(),
                                  scope: scope.clone() }],
               &rc_memory::BatchCtx { source_agent: "rc-derive".into(), device_id: "unknown".into() },
               NOW).unwrap();
    let head = m.conn().query_row("SELECT id FROM card WHERE kind='causal'", [],
                                  |r| r.get::<_, String>(0)).unwrap();

    let text = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap().text();
    let lines: Vec<&str> = text.lines().collect();

    // 链头本身进导出件（`card:<head>` causal；链头行 canonical 之后落 `card`），两条环节挂在它下面。
    assert!(text.contains(&format!("`card:{head}` causal")),
        "链头那一行没进导出件 ⇒ 环节渲染根本没有宿主：{text}");
    let la = format!("    ⇒ link card:{a} (human)");
    let lb = format!("    ⇒ link card:{b} (human)");
    let ia = lines.iter().position(|l| *l == la);
    let ib = lines.iter().position(|l| *l == lb);
    assert!(ia.is_some(), "环节甲没按第四形渲出来：{text}");
    assert!(ib.is_some(), "环节乙没按第四形渲出来：{text}");
    assert!(ia < ib, "环节必须按 ord 序（甲 0 先、乙 1 后）：甲在第 {:?} 行、乙在第 {:?} 行", ia, ib);

    // 环节不是依据：这一份导出件里链头不该被 `provenance_chain` 渲成依据行 `    ← card:…`
    //（被引卡 a、b 自己有 turn 出处 ⇒ 它们各自渲的是 `    ← turn …`，不是 `    ← card:…`）。
    assert!(lines.iter().all(|l| !l.starts_with("    ← card:")),
        "链头的环节被当依据渲了出去 ⇒ 导出面出现 Basis 假话（R3 那一格的导出证人）：{text}");

    // 守卫 12：环节那一行只给句柄与角色，被引行的正文一个字都不带。
    let link_lines: Vec<&&str> = lines.iter().filter(|l| l.starts_with("    ⇒ link")).collect();
    assert_eq!(link_lines.len(), 2, "链头两条环节 ⇒ 恰两行 `⇒ link`：{text}");
    for l in link_lines {
        assert!(!l.contains(a_body) && !l.contains(b_body),
            "环节渲染把被引行的正文带进了导出件：{l}");
    }
    // 反证夹具前提：两张被引卡的正文**确实**在导出件里出现（走的是各自那一行的 `正文` 那句，不是环节）——
    // 少了这两句，上面那个 `!contains(body)` 会因"整份导出件根本没渲正文"而空绿。
    assert!(text.contains(a_body) && text.contains(b_body),
        "夹具前提破了：被引卡的正文应各自出现在它那一行，否则守卫 12 那两句是空断言");
}

/// 批次 M 段 C · 判据 56 的行为臂：导出件把**链头类型**渲出来，空值念 `unknown`。
///
/// 形状 `    ⇒ type=<v>`（四空格 + `⇒ type=` + 值），位置在 `    ⇒ link` 之前；NULL/非链 ⇒
/// `type=unknown`（与时间线 `time_confidence=unknown` 同族，空值那一支不另立形状）。
/// 两条两环节链各一个类型：`causes` 与 `prevents`，后者的列值被 UPDATE 成 NULL（CHECK 允许 NULL，
/// 模拟老行/未给出）⇒ 同一份导出件里两种读音并存，写死 `unknown` 或写死某个值都红。
/// **守卫 12**：`⇒ type` 那一行只有类型值，被引行的正文一个字都不带。
/// 变异电池 M-C2：把 `ct.as_deref().unwrap_or("unknown")` 整段改成恒 `"unknown"`
/// ⇒ 下面 `type=causes` 那一格红。
#[test]
fn 导出件把链头类型渲出来_空值念_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "p-type".into() };
    let turn = |c: char| c.to_string().repeat(64);
    let human = |c: char| vec![Provenance { turn_uid: turn(c), anchor_role: AnchorRole::Human }];
    let a_body = "类型标本甲独有正文-KT41A";
    let b_body = "类型标本乙独有正文-KT41B";
    let a = put_card(m, &FactContent { kind: "decision".into(), title: "类型环节甲".into(),
                        body: a_body.into(), scope: scope.clone(), valid_to_ms: None },
                     &human('a'), NOW).unwrap().id;
    let b = put_card(m, &FactContent { kind: "decision".into(), title: "类型环节乙".into(),
                        body: b_body.into(), scope: scope.clone(), valid_to_ms: None },
                     &human('b'), NOW).unwrap().id;
    // 两条链只差机制句与类型：机制句进链头 id 的 preimage，所以两条链各有自己的头行。
    let mech_causes = "类型标本第一条链的机制句";
    let mech_prevents = "类型标本第二条链的机制句";
    put_chains(m, &[
        ChainContent { mechanism: mech_causes.into(), link_ids: vec![a.clone(), b.clone()],
                       basis_ids: vec![], causal_type: "causes".into(), scope: scope.clone() },
        ChainContent { mechanism: mech_prevents.into(), link_ids: vec![a.clone(), b.clone()],
                       basis_ids: vec![], causal_type: "prevents".into(), scope: scope.clone() },
    ], &rc_memory::BatchCtx::synthetic(), NOW).unwrap();
    let head_of = |mech: &str| -> String {
        m.conn().query_row("SELECT id FROM card WHERE kind='causal' AND text=?1", [mech],
                           |r| r.get::<_, String>(0)).unwrap()
    };
    let head_prevents = head_of(mech_prevents);
    // CHECK 允许 NULL ⇒ 这一句造得出"老行/未给出"那一格；id 是 sha256 hex，拼进 SQL 没有引号风险。
    m.conn().execute_batch(&format!("UPDATE card SET causal_type=NULL WHERE id='{head_prevents}'"))
        .unwrap();

    let text = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap().text();
    let lines: Vec<&str> = text.lines().collect();
    // 一格：两种读音各自恰好一次（写死任一边都红）。
    assert_eq!(text.matches("    ⇒ type=causes").count(), 1,
        "非空类型没按 `⇒ type=<v>` 渲出来（或被渲了两次）：{text}");
    assert_eq!(text.matches("    ⇒ type=unknown").count(), 1,
        "NULL 那一支没念 `unknown`（或被渲了两次）：{text}");
    // 第二格：这一形只对 `kind='causal'` 的链头行出现 ⇒ 恰好两行（普通卡/事件一行都不多）。
    let type_lines: Vec<&&str> = lines.iter().filter(|l| l.starts_with("    ⇒ type=")).collect();
    assert_eq!(type_lines.len(), 2,
        "`⇒ type=` 行恰好两行（两条链头），多出来的那一行是普通行被带上了这一形：{text}");
    // 第三格：位置在 `⇒ link` 之前（同一头行的 type 行紧邻着它的第一行环节）。
    let it = lines.iter().position(|l| *l == "    ⇒ type=causes")
        .expect("type=causes 那一行必须在场");
    assert!(lines.get(it + 1).is_some_and(|l| l.starts_with("    ⇒ link")),
        "`⇒ type` 没排在 `⇒ link` 之前（或环节根本没渲）：{text}");
    // 守卫 12：type 行只有类型值，被引行的正文一个字都不带。
    for l in type_lines {
        assert!(!l.contains(a_body) && !l.contains(b_body),
            "type 那一行把被引行的正文带进了导出件：{l}");
    }
}

#[test]
fn 导出件把时间行的软边渲出来() {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.memory();
    let scope = Scope { kind: ScopeKind::Project, key: "p-export-link".into() };
    let human = |c: char| vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }];
    let meeting = put_event(m, &FactContent {
        kind: "meeting".into(), title: "导出会".into(), body: "会。".into(),
        scope: scope.clone(), valid_to_ms: None }, &human('a'), NOW).unwrap();
    let basis = put_card(m, &FactContent {
        kind: "decision".into(), title: "导出决定".into(), body: "决定。".into(),
        scope: scope.clone(), valid_to_ms: None }, &human('b'), NOW).unwrap();
    put_timeline_rows(m, &[TimelineContent {
        title: "derive-导出软边".into(), body: "导出软边".into(), scope,
        time_anchor_ms: Some(NOW + 1), time_anchor_id: Some(meeting.id.clone()),
        time_confidence: "anchored".into(), basis_ids: vec![basis.id.clone()],
    }], &rc_memory::BatchCtx::synthetic(), NOW).unwrap();

    let text = rc_agent::export::render_export(m, &rc_agent::export::ExportRequest::all(NOW).unwrap())
        .unwrap().text();
    // 软边的 from 端是整句柄（`card:<id>`），导出件原样渲出来（不自己拼 `event:` 前缀）。
    assert!(text.contains(&format!("    ⇒ event_link {} evidence=[\"{}\"]",
                                   meeting.key_string(), basis.id)),
            "时间行的软边没渲出来：{text}");
}

