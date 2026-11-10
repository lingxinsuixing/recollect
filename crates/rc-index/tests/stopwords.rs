//! G10 第一条腿的程序钉子：停用词小表的阴性对照、空查询形状、引号逃逸口、真词保底。
//!
//! 验收判据（顺序不可换）的落点：
//! ① 阴性对照：「毛姆的小说」类虚词查询在停用词方案下 **0 命中**——且夹具先证明
//!    自己带着审计里的病（旧口径的 OR 串对这些行必命中），红绿灯才携带信息；
//! ② 真词不被吞：内容词照常命中，`"的"` 走引号逃逸口照常匹配；
//! ③ 纯虚词查询的形状 = `NoQueryTokens`（枚举化的 0 行，不许全表扫）。
//!
//! 夹具全部合成（守卫 12）：编造的中文短句，无 CANARY、无路径、无密钥。

use jieba_rs::Jieba;
use rc_index::analyzer::query_tokens;
use rc_index::rebuild::{rebuild, search, IndexError};
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};

const NOW: i64 = 1_700_000_000_000;

/// `(slug, title, body)`。前两行是审计里「的」假命中的载体形状（含独立成词的「的」）；
/// 第三行是内容词行——**不含** 毛姆/小说：阴性对照的前提就是库里真没有（审计原话
/// 「应为 no_rows」），0 命中必须是"真没有"，不许靠夹具里的同词行假绿。
const ROWS: &[(&str, &str, &str)] = &[
    ("amd_quota", "AMD 的配额说明", "AMD 的调用配额与 142 次"),
    ("sched", "排期的脚本清单", "每天排期的脚本都会跑"),
    ("book", "散文书单", "读书笔记与书单整理"),
];

fn memory_handle() -> (tempfile::TempDir, MemoryHandle) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.into_memory();
    let prov = Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Agent };
    for (_, title, body) in ROWS {
        let fc = FactContent {
            kind: "decision".into(),
            title: (*title).into(),
            body: (*body).into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-g10".into() },
            valid_to_ms: None,
        };
        put_card(&m, &fc, std::slice::from_ref(&prov), NOW).unwrap();
    }
    rebuild(&m, NOW).unwrap();
    (dir, m)
}

/// 旧口径（无停用词）的查询词串：jieba 切词 + 归一 + 去重，**不过滤虚词**。
/// 复现的是 2026-11-07 之前 analyzer 的行为，用来证"夹具真的带着病"。
fn old_query_tokens(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    Jieba::new()
        .cut(text, false)
        .into_iter()
        .map(|t| {
            t.word.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect::<String>()
        })
        .filter(|s| !s.is_empty() && s.chars().any(|c| c.is_alphanumeric()))
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

#[test]
fn 阴性对照_毛姆的小说_停用词方案下零命中() {
    let (_d, m) = memory_handle();

    // 先证夹具带着病：旧口径的 OR 串（含「的」）在影子表上必命中。
    let old = old_query_tokens("毛姆的小说");
    assert!(old.contains(&"的".to_string()), "旧口径要切出虚词「的」，实际 {old:?}");
    let old_match = old
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ");
    let diseased: i64 = m
        .conn()
        .query_row(
            "SELECT count(*) FROM rc_shadow WHERE rc_shadow MATCH ?1",
            rusqlite::params![old_match],
            |r| r.get(0),
        )
        .unwrap();
    assert!(diseased > 0, "夹具没复现审计里的病（旧口径应命中 ≥1 行），下面的 0 命中恒真");

    // 验收 ①：新口径（查询侧停用词）下，虚词不再进 MATCH ⇒ 0 行。
    let toks = query_tokens("毛姆的小说");
    assert!(!toks.is_empty(), "过滤后成空集就走 NoQueryTokens 形状了，不是这条要证的");
    assert!(!toks.iter().any(|t| t == "的"), "虚词「的」必须被滤掉：{toks:?}");
    assert!(search(&m, "毛姆的小说").unwrap().is_empty(), "阴性对照失败：虚词查询仍有命中");
}

#[test]
fn 纯虚词查询是枚举化的零行_不许全表扫() {
    let (_d, m) = memory_handle();
    assert!(query_tokens("的了是").is_empty(), "纯虚词过滤后应为空集");
    let err = search(&m, "的了是").expect_err("纯虚词查询不许退化成全表扫");
    assert!(matches!(err, IndexError::NoQueryTokens), "错误面贴错了：{err:?}");
}

#[test]
fn 引号逃逸口_停用词包进引号就当关键词查() {
    let (_d, m) = memory_handle();
    assert_eq!(query_tokens("\"的\""), vec!["的".to_string()], "受保护的虚词必须存活");
    // 影子表里确实有以「的」为词的行（夹具第一行），受保护的「的」照常走 FTS。
    let hits = search(&m, "\"的\"").unwrap();
    assert!(!hits.is_empty(), "引号逃逸口断了：受保护的虚词查不到东西");
    // 不成对的引号按无引号处理：过滤不被一个漏引号关掉。
    assert!(query_tokens("\"的了是").is_empty(), "漏引号不许把停用词过滤整个放行");
}

#[test]
fn 真词不被停用词吞掉() {
    let (_d, m) = memory_handle();
    let sched = search(&m, "排期").unwrap();
    assert_eq!(sched.len(), 1, "内容词「排期」应恰好命中排期行：{:?}", sched.len());
    let book = search(&m, "书单").unwrap();
    assert_eq!(book.len(), 1, "内容词「书单」应恰好命中书单行");
    // 混合查询：真词承载命中，虚词只是不再添乱。
    let mixed = search(&m, "排期的脚本").unwrap();
    assert_eq!(mixed.len(), 1, "「排期的脚本」应靠内容词命中排期行");
}
