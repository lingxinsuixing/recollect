//! W-0（派发档 I-5b §0.5）：MF-1 在**注入面**的最后一格消费口。
//!
//! `surface.rs` 的两行（BASE 上是 `:19` 与 `:26`）过去只读 `d.facts.as_of_ms`，那正是
//! hook 注入里 `[recollect] card:… | as_of=…` 这一行的产地。episode 镜像卡在成员全无可知
//! 事件时间时，`as_of_ms` 落的是**发话时刻**（`writers_x::episode_time_arm` 的第二支，
//! 库里那一格是 `'speech'`）——那一面当时会把"那个人说过这话的时间"说成"这件事发生的时间"。
//!
//! 本文件钉三格，缺一格就不算接上：
//! 1. `speech` 那一支在注入面**说得出**"这是发话时刻、事件时间未知"，而且**不出现**"发生在"；
//! 2. `anchored` 与 `unknown`/NULL/域外串两支持现字节**一字不动**（这一面喂的是 hook 的注入行，
//!    形状一变就是改注入契约）——判据写成一个等式：与 BASE 那两行的 `format!` 逐字相同；
//! 3. 行账不变：`speech` 那一支仍然只占它那一档的物理行（signpost 一行 / full 两行），
//!    围栏前缀与 `| as_of=` 这个机读键都在原位。
//!
//! **判据不另造**：时间档的归一住在 `recall::time_confidence_slug`（值域、NULL、域外串怎么算
//! 全在那一格），本文件断的是"注入面有没有去读那一列、读了之后怎么说"。
//! 第四条用例就是这一句的反面检查：把一张**普通 decision 卡**的那一列改成 `'speech'`，
//! 那一行照样要说"发话时刻"——这一面认的是**列**，不是 `kind='episode'`。
//!
//! 夹具的 `speech` 与 `anchored` 两支都走**真生产者**（`put_episodes`；成员有没有
//! `occurred_at_ms` 决定落哪一支），不是手搓 SQL 造出来的读数。

use rc_agent::surface::{probe_block, probe_via_search, FENCE};
use rc_memory::keys::{AnchorRole, MemKey, Verdict};
use rc_memory::owner::{OwnerCtx, Visibility};
use rc_memory::writers_x::{
    put_card, put_entity, put_episodes, BatchCtx, EntityContent, EpisodeContent, FactContent,
};
use rc_memory::{put_fact_rows_with_extras, FactExtras, FactWriteItem, Provenance, Scope, ScopeKind};
use rc_store::Root;

const NOW: i64 = 1_700_000_000_000;
const MIN: i64 = 60_000;
/// 只出现在**用例指名的那几张卡的标题**里的词：探针那一句查询点的就是这几张。
const Q: &str = "注入面时间档合成词";

fn scope() -> Scope {
    Scope { kind: ScopeKind::Project, key: "proj-synthetic".into() }
}

fn ctx() -> BatchCtx {
    BatchCtx::synthetic()
}

fn prov(c: char) -> Vec<Provenance> {
    vec![Provenance { turn_uid: c.to_string().repeat(64), anchor_role: AnchorRole::Human }]
}

fn turn(index: usize) -> String {
    format!("{:064x}", 0x51 + index)
}

/// 归属与 `BatchCtx::synthetic()` 逐字同串（与 `replica_mode.rs` 头顶那条理由同一格：
/// 名字不对就不是实现有问题，是夹具的读者身份不对）。
fn owner() -> OwnerCtx {
    OwnerCtx {
        owner_principal_id: Some("p-surface".into()),
        owner_agent_id: "synthetic-agent".into(),
        visibility: Visibility::Project,
        retention: "keep".into(),
    }
}

struct Fix {
    _dir: tempfile::TempDir,
    root: Root,
}

impl Fix {
    fn m(&self) -> &rc_store::MemoryHandle {
        self.root.memory()
    }
}

fn fix() -> Fix {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    // 一条真锚（fact 的主语必须存在于 `anchor` 表里），标题不带 `Q`。
    put_entity(root.memory(), &EntityContent {
        kind: "person".into(), title: "注入面主语".into(), scope: scope(), aliases: vec![],
    }, &prov('e'), NOW).unwrap();
    Fix { _dir: dir, root }
}

fn subject_id(m: &rc_store::MemoryHandle) -> String {
    m.conn().query_row("SELECT id FROM anchor ORDER BY created_at_ms, id LIMIT 1", [],
        |r| r.get(0)).unwrap()
}

/// 一条真 fact（自带陈述面消费卡）。`text` 是要不要带 `Q` 那一格由调用方决定：
/// 经历那一族不带（探针要命中的是镜像卡），NULL 那一支要带（否则循环是空过的）。
fn fact(fix: &Fix, index: usize, value: &str, text: &str) -> String {
    let item = FactWriteItem {
        subject_anchor_id: subject_id(fix.m()),
        object_anchor_id: None,
        predicate: "做了".into(),
        value: Some(value.into()),
        category: "event".into(),
        mode: "stated".into(),
        time_text: String::new(),
        quote: value.into(),
        evidence: vec![(turn(index), AnchorRole::Human)],
        text: text.into(),
        scope: scope(),
        source_ms: NOW + index as i64 * MIN,
    };
    let extras = FactExtras { participants: vec![], ..Default::default() };
    let out = put_fact_rows_with_extras(fix.m(), &[item], &[extras], &ctx(), &owner(), NOW).unwrap();
    assert_eq!(out.facts, 1, "夹具的 fact 必须真的落库：{value}");
    format!("card:{}", out.card_ids[0].id)
}

fn fact_id_of(fix: &Fix, card_handle: &str) -> String {
    let id = card_handle.split_once(':').map(|(_, i)| i).unwrap_or(card_handle);
    let json_str: String = fix.m().conn().query_row(
        "SELECT pointers_json FROM card WHERE id=?1", [&id], |r| r.get(0)).unwrap();
    for part in json_str.trim_matches(|c| c == '[' || c == ']').split(',') {
        if let Some(f) = part.trim().trim_matches('"').strip_prefix("fact:") {
            return f.to_string();
        }
    }
    panic!("夹具的成员卡必须点回一条 fact：{card_handle} / {json_str}");
}

/// 一段两成员的经历；`dated` 决定成员有没有事件时间 ⇒ 镜像卡落 `anchored` 还是 `speech`
/// （`writers_x::episode_time_arm` 的两支，写侧那一条口径在这里原样可达）。
fn episode(fix: &Fix, dated: bool) -> MemKey {
    let a = fact(fix, 0, "调通闸", "甲某在经历里调通闸");
    let b = fact(fix, 1, "补了桩", "甲某在经历里补了桩");
    let ids = vec![fact_id_of(fix, &a), fact_id_of(fix, &b)];
    if dated {
        for (i, id) in ids.iter().enumerate() {
            fix.m().conn().execute_batch(&format!(
                "UPDATE fact SET occurred_at_ms={} WHERE id='{}'",
                NOW + (i as i64 + 10) * MIN, id)).unwrap();
        }
    }
    let out = put_episodes(fix.m(), &[EpisodeContent {
        title: format!("{Q}的那段经历"),
        narrative: "先调通闸，再补了桩。".into(),
        fact_ids: ids,
    }], &ctx(), NOW).unwrap();
    assert_eq!(out.episodes, 1, "经历必须真落库：{out:?}");
    let card_id: String = fix.m().conn().query_row(
        "SELECT id FROM card WHERE kind='episode' ORDER BY created_at_ms, id LIMIT 1",
        [], |r| r.get(0)).unwrap();
    MemKey::parse(&format!("card:{card_id}")).unwrap()
}

/// 生产路径上的一次探针：建索引 → `probe_via_search` → `probe_block`。
/// 返回**注入块的每一行**与 `Decision::lines()` 的行账（两条一起比，就是"渲染器的行数
/// 与判定的行账不许脱钩"那一条既有判据在本文件里的形状）。
fn injected(fix: &Fix) -> (Vec<String>, usize) {
    rc_index::rebuild(fix.m(), NOW).unwrap();
    let view = probe_via_search(fix.m(), None, Q, NOW).unwrap();
    assert!(!view.forms.is_empty(), "探针面上一个候选都没有 ⇒ 查询词没命中夹具");
    let accounted = view.forms.iter().map(|d| d.lines()).sum();
    (probe_block(&view).text().lines().map(|l| l.to_string()).collect(), accounted)
}

fn line_for(lines: &[String], key: &MemKey) -> String {
    let k = key.key_string();
    lines.iter().find(|l| l.contains(&k))
        .unwrap_or_else(|| panic!("注入块里没有这一行 {k}：{lines:?}"))
        .clone()
}

/// **① `speech` 那一支（W-0 的命门）**：镜像卡的钟是发话钟，注入面必须说得出这件事。
/// 这里给镜像卡打上 `verdict=Passed`，所以它落在 **full 档**（代言那一支）——
/// 那一档才会开口替这条记忆说话，把发话钟当成事件钟说出去的伤害也最大。
#[test]
fn speech_那一支在注入面说得出这是发话钟() {
    let fix = fix();
    let mirror = episode(&fix, false);
    rc_memory::set_verdict(fix.m(), &mirror, Verdict::Passed, NOW).unwrap();
    let (lines, accounted) = injected(&fix);
    let line = line_for(&lines, &mirror);
    assert!(line.contains("as_of="), "`as_of=` 这个机读键必须还在原位：{line}");
    assert!(line.contains("发话时刻"), "没说得出这是发话钟：{line}");
    assert!(line.contains("事件时间未知"), "没明写事件时间未知：{line}");
    assert!(!line.contains("发生在"), "把发话钟说成了事件发生：{line}");
    // 行账：speech 那一支只换时间格的写法，**不加行**（`fit` 的上限与真实 stdout 不许脱钩）。
    assert_eq!(lines.len(), accounted, "渲染行数与 Decision::lines() 的账不符：{lines:?}");
    assert!(lines.iter().all(|l| l.starts_with(FENCE)), "围栏不是每行前缀（V3）：{lines:?}");
    // full 档仍是"正文行 + 句柄行"：句柄行前面那一行是正文行，它不带时间格。
    let at = lines.iter().position(|l| l.contains(&mirror.key_string())).expect("句柄行");
    assert!(at >= 1 && !lines[at - 1].contains("as_of="),
        "full 档的两行形状变了：{lines:?}");
    // 与 BASE 原文的逐字对照：时间格那一格之外一个字都不许多（`as_of` 取库里那一行的读数）。
    let view = probe_via_search(fix.m(), None, Q, NOW).unwrap();
    let d = view.forms.iter().find(|x| x.key == mirror).expect("镜像卡在候选里");
    assert_eq!(line,
        format!("{FENCE}{} | as_of={}（发话时刻，事件时间未知）",
                d.key.key_string(), d.facts.as_of_ms));
}

/// **② `anchored` 一字不动**：那一格本来就是事件时间，注入面沿用 BASE 那两个 `format!` 之一
/// （signpost 那一支：`句柄 | 标题 | as_of=<ms>`）。等式右边就是 BASE `surface.rs` 的 `probe_block` signpost 臂那行 `format!` 的原文。
#[test]
fn anchored_那一支在注入面与_base_逐字节相同() {
    let fix = fix();
    let mirror = episode(&fix, true);
    let (lines, accounted) = injected(&fix);
    let line = line_for(&lines, &mirror);
    let view = probe_via_search(fix.m(), None, Q, NOW).unwrap();
    let d = view.forms.iter().find(|x| x.key == mirror).expect("镜像卡在候选里");
    assert_eq!(line,
        format!("{FENCE}{} | {} | as_of={}", d.key.key_string(), d.facts.title, d.facts.as_of_ms),
        "anchored 那一支持现字节不许变（这是注入契约）：{line}");
    assert!(!line.contains("发话"), "有事件时间却说发话钟：{line}");
    assert_eq!(lines.len(), accounted, "行账脱钩：{lines:?}");
}

/// **②' `unknown` / NULL / 值域外的串**三支都走既有那一支：注入面**认不出**这三种读数里
/// 有任何差别（归一在 `recall::time_confidence_slug` 那一格就把后两者收进 `unknown`），
/// 而 `unknown` 那一支在 BASE 上本来就是 `as_of=<ms>`。
#[test]
fn unknown_null_与域外串_三支都保持既有注入字节() {
    let fix = fix();
    // `put_card` 那一族显式写 `'unknown'`（`writers_x::card_sql`）。
    let unknown = put_card(fix.m(), &FactContent {
        kind: "decision".into(), title: format!("{Q}的甲"), body: "定了十点发布。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('u'), NOW).unwrap();
    // NULL：`writers_fact::CARD_INSERT` 那一族的最小消费卡，列单里根本没有这一列。
    let null_card = fact(&fix, 6, "没有这一列的行", &format!("{Q}的乙"));
    // 值域外的串：`card.time_confidence` 是裸 TEXT、没有 CHECK（`ddl.rs` 的 `card` 建表里那一列）。
    let garbage = put_card(fix.m(), &FactContent {
        kind: "decision".into(), title: format!("{Q}的丙"), body: "域外值的一行。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('g'), NOW).unwrap();
    let garbage_id = garbage.id.clone();
    fix.m().conn().execute_batch(&format!(
        "UPDATE card SET time_confidence='一段根本不是那几个词的文字' WHERE id='{garbage_id}'"))
        .unwrap();
    rc_index::rebuild(fix.m(), NOW).unwrap();
    let view = probe_via_search(fix.m(), None, Q, NOW).unwrap();
    let text = probe_block(&view).text();
    let mut covered = 0usize;
    for d in &view.forms {
        let line = match text.lines().find(|l| l.contains(&d.key.key_string())) {
            Some(l) => l.to_string(),
            None => continue,
        };
        covered += 1;
        assert_eq!(line,
            format!("{FENCE}{} | {} | as_of={}", d.key.key_string(), d.facts.title,
                    d.facts.as_of_ms),
            "unknown/NULL/域外三支的现字节不许变：{line}");
        assert!(!line.contains("发话"), "非 speech 的一支却说发话钟：{line}");
        assert!(!line.contains("一段根本不是那几个词"),
            "域外原串被回显进注入行（守卫 12）：{line}");
    }
    // 三支里至少要有一张 unknown 和一张 NULL 在候选面上，否则上面那个循环是空过的。
    assert!(view.forms.iter().any(|d| d.key == unknown), "unknown 那张要在候选面上");
    assert!(view.forms.iter().any(|d| d.key.key_string() == null_card),
        "NULL 那张要在候选面上");
    assert!(view.forms.iter().any(|d| d.key == garbage), "域外值那张要在候选面上");
    assert!(covered >= 3, "循环只比了 {covered} 行，三支没跑齐");
}

/// **③ 这一面认的是那一列，不是 `kind`**：一张普通 `decision` 卡的那一列被改成 `'speech'`
/// （写侧今天只有 episode 那一族会落这个值，这里改列是为了把判据从"卡族"上摘下来），
/// 注入行照样要说"发话时刻"。反过来，`kind='episode'` 而那一列是 `anchored` 的那一张
/// （第二条用例）一个字都没多说。
#[test]
fn 这一面认的是那一列_而不是_kind_episode() {
    let fix = fix();
    let card = put_card(fix.m(), &FactContent {
        kind: "decision".into(), title: format!("{Q}的丁"), body: "普通卡被改成发话钟。".into(),
        scope: scope(), valid_to_ms: None,
    }, &prov('s'), NOW).unwrap();
    let id = card.id.clone();
    fix.m().conn().execute_batch(&format!(
        "UPDATE card SET time_confidence='speech' WHERE id='{id}'")).unwrap();
    rc_index::rebuild(fix.m(), NOW).unwrap();
    let view = probe_via_search(fix.m(), None, Q, NOW).unwrap();
    let d = view.forms.iter().find(|x| x.key == card).expect("那张普通卡在候选面上");
    assert_ne!(d.facts.kind, "episode", "夹具要的不是经历卡，而是被改过那一列的普通卡");
    let line = probe_block(&view).text().lines()
        .find(|l| l.contains(&card.key_string())).expect("注入块里有这一行").to_string();
    assert!(line.contains("发话时刻") && line.contains("事件时间未知"),
        "这一面没去读那一列，只在读 kind：{line}");
    assert!(!line.contains("发生在"), "{line}");
}
