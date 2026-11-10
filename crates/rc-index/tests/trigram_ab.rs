//! G10 第二条腿：FTS5 trigram 分词旁路 A/B（审计报告 16 条探针 + 阴性对照组）。
//!
//! 旁路纪律：trigram 索引是**随本测试创建、随 TempDir 消亡**的实验表（`rc_tri_shadow`），
//! 生产 `rc_shadow` 的 schema/重建流程一个字不动；不落盘、不进任何 DDL 面。
//! 夹具按审计探针表合成（守卫 12：编造中文短句，无 CANARY/路径/密钥）——
//! 读数是**夹具上的相对对比**，不是现网复跑；真值标注来自审计报告的探针真值列。
//!
//! 四臂（读数带臂名；本文件所属 commit 见派发报告）：
//! - `A0` 旧口径：查询侧无停用词的 jieba OR（复现 2026-11-07 之前的行为）；
//! - `A1` 停用词方案：生产口径 `analyzer::query_tokens`（走 `rc_index::search` 全链）；
//! - `B1` trigram 整句短语：查询原文整体作 trigram 短语 MATCH（= 逐字连读子串匹配）；
//! - `B2` trigram 分词 OR：A1 词元里 ≥3 字（trigram 最短长度）的词各自作短语 OR。
//!
//! 跑法：`cargo test -p rc-index --test trigram_ab -- --nocapture`
//!
//! 已知的 trigram 边界（如实钉进断言，不藏在结论里）：
//! - MATCH 词元 <3 个 Unicode 字符 ⇒ **永远 0 行**（3-gram 索引的根本限制）——
//!   中文里最常见的 2 字词（示例主人/水位/备份）天然查不到；
//! - 短语匹配是**逐字连读**子串语义：查询词序与原文不一致、或中间隔字，就不命中。
use jieba_rs::Jieba;
use rc_index::analyzer::query_tokens;
use rc_index::rebuild;
use rc_index::IndexError;
use rc_memory::{put_card, AnchorRole, FactContent, Provenance, Scope, ScopeKind};
use rc_store::{MemoryHandle, Root};
use rusqlite::Connection;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const NOW: i64 = 1_700_000_000_000;
/// 旁路实验表名（带 `rc_tri_` 前缀表示"实验、非生产 schema"，随 TempDir 消亡）。
const TRI: &str = "rc_tri_shadow";

/// 探针真值标注（来自 2026-11-07 recall-audit 探针表）：
/// `Truth` = 真值在库（relevant 列出承载真值的行）；
/// `NoTruth` = 审计判定"库中无/泛问无路由"（期望 0 真命中，命中即噪声）；
/// `Negative` = 阴性对照（期望所有臂 0 行）。
#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Truth,
    NoTruth,
    Negative,
}

struct Probe {
    q: &'static str,
    relevant: &'static [&'static str],
    kind: Kind,
}

const PROBES: &[Probe] = &[
    Probe { q: "示例主人是谁", relevant: &["person_owner", "nickname"], kind: Kind::Truth },
    Probe { q: "示例昵称是什么", relevant: &["nickname"], kind: Kind::Truth },
    Probe { q: "W31 是什么状态", relevant: &["w31_done", "w31_causal"], kind: Kind::Truth },
    Probe { q: "上线进展", relevant: &[], kind: Kind::NoTruth },
    Probe { q: "为什么砍掉调用配额", relevant: &[], kind: Kind::NoTruth },
    Probe { q: "batch8 为什么放弃", relevant: &["batch8"], kind: Kind::Truth },
    Probe { q: "今天早上发生了什么", relevant: &[], kind: Kind::NoTruth },
    Probe { q: "昨天的抽干结果", relevant: &["drain_task"], kind: Kind::Truth },
    Probe { q: "用户拍板过什么", relevant: &[], kind: Kind::NoTruth },
    Probe { q: "谁负责备份", relevant: &["backup"], kind: Kind::Truth },
    Probe { q: "还有什么没修", relevant: &["fix_order"], kind: Kind::Truth },
    Probe { q: "下一步计划", relevant: &["plan_next"], kind: Kind::Truth },
    Probe { q: "rc=2 修复和什么有关", relevant: &["w31_done", "w31_causal"], kind: Kind::Truth },
    Probe { q: "水位漂移怎么发现的", relevant: &["watermark"], kind: Kind::Truth },
    Probe { q: "最近发生了什么", relevant: &[], kind: Kind::NoTruth },
    // —— 阴性对照组 ——
    Probe { q: "毛姆的小说", relevant: &[], kind: Kind::Negative },
    Probe { q: "三体的读后感", relevant: &[], kind: Kind::Negative },
    Probe { q: "周杰伦的演唱会", relevant: &[], kind: Kind::Negative },
];

/// `(slug, title, body)`。三段来源：
/// ① 真值卡——审计探针表「库内命中」列的合成对应物；
/// ② 噪声卡——审计里被点名的误命中来源（AMD 配额、8 张 rc-* 文件锚、虚词填充卡），
///    虚词填充卡复现「的」假命中 ~20 行的库形；
/// ③ 短语正控卡——给 trigram 一条"逐字连读必命中"的 sanity 行。
const ROWS: &[(&str, &str, &str)] = &[
    // —— 真值卡 ——
    ("person_owner", "示例主人", "项目的主人身份行"),
    ("nickname", "示例昵称", "B 站昵称，与示例主人同一主体"),
    ("decide_retry", "示例主人决定重试抽干", "抽干失败后重试的裁定"),
    ("w31_done", "W31 已落地", "rc=2 修复收尾，测试全绿"),
    ("w31_causal", "派单 W31", "rc=2 修复，r29 r28 因果链"),
    ("batch8", "batch8 已放弃", "300 秒墙，长轮撞墙后放弃"),
    ("fix_order", "修复顺序", "先修 1 再 3 再 2 再 4，5 缺陷待修"),
    ("watermark", "review_item id=211 水位计数 0", "水位漂移由对账读数发现"),
    ("drain_task", "抽干任务", "tail.exe 与计划任务重注册"),
    ("backup", "备份座位", "W28 备份进日路径"),
    ("plan_next", "下一步计划", "P2-6 排入 W34"),
    // —— 噪声卡：审计点名的误命中来源 ——
    ("amd_quota", "AMD 的配额说明", "AMD 的调用配额与 142 次"),
    ("rc_anchor_1", "rc-index 的 analyzer", "索引层文件锚"),
    ("rc_anchor_2", "rc-search 的 score", "检索层文件锚"),
    ("rc_anchor_3", "rc-store 的 ddl", "存储层文件锚"),
    ("rc_anchor_4", "rc-memory 的 reader", "记忆层文件锚"),
    ("rc_anchor_5", "rc-agent 的 tools", "工具层文件锚"),
    ("rc_anchor_6", "rc-distill 的 dedupe", "去重层文件锚"),
    ("rc_anchor_7", "rc-mcp 的 main", "入口文件锚"),
    ("rc_anchor_8", "rc-cycle 的 lib", "循环层文件锚"),
    // —— 虚词填充卡：复现审计「的」假命中 20 行的库形 ——
    ("filler_01", "会议的纪要一", "例会的纪要归档"),
    ("filler_02", "通知的草稿", "站内信的通知草稿"),
    ("filler_03", "周报的汇总", "项目的周报汇总条目"),
    ("filler_04", "日志的轮转", "运行的日志轮转策略"),
    ("filler_05", "清单的核对", "入库清单的核对条目"),
    ("filler_06", "笔记的摘录", "摘录的读书段落"),
    ("filler_07", "配置的导出", "环境配置的导出记录"),
    ("filler_08", "日程的提醒", "日程提醒的触发器"),
    ("filler_09", "账目的核对", "月度账目的核对结果"),
    ("filler_10", "模板的替换", "邮件模板的占位符"),
    ("filler_11", "镜像的同步", "仓库镜像的同步任务"),
    ("filler_12", "页面的样式", "落地页的样式调整"),
    ("filler_13", "队列的积压", "消息队列的积压读数"),
    ("filler_14", "缓存的过期", "缓存键的过期策略"),
    // —— 短语正控卡：trigram「逐字连读」语义的 sanity 载体 ——
    ("sched_script", "排期脚本每晚跑", "排期脚本已接入日路径"),
];

#[derive(Clone, Copy, Default)]
struct Arm {
    rows: usize,
    true_hits: usize,
    false_hits: usize,
    us: u128,
}

impl Arm {
    fn record(&mut self, keys: &[String], relevant: &[String], us: u128) {
        self.rows = keys.len();
        self.us = us;
        for k in keys {
            if relevant.contains(k) {
                self.true_hits += 1;
            } else {
                self.false_hits += 1;
            }
        }
    }
}

/// 旧口径（无停用词）的查询词：jieba 切词 + 归一 + 去重。复现 2026-11-07 之前的行为。
/// 词典进程内一份（`Jieba::new()` 每次 100ms 级，进计时就毁了 A0 臂的读数）。
fn jieba() -> &'static Jieba {
    static J: std::sync::OnceLock<Jieba> = std::sync::OnceLock::new();
    J.get_or_init(Jieba::new)
}

fn old_query_tokens(text: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    jieba()
        .cut(text, false)
        .into_iter()
        .map(|t| {
            t.word.chars().filter(|c| !c.is_whitespace()).flat_map(char::to_lowercase).collect::<String>()
        })
        .filter(|s| !s.is_empty() && s.chars().any(|c| c.is_alphanumeric()))
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

fn or_match(tokens: &[String]) -> String {
    tokens
        .iter()
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn fts_keys(conn: &Connection, table: &str, match_str: &str) -> Vec<String> {
    let mut st = conn
        .prepare(&format!("SELECT key FROM {table} WHERE {table} MATCH ?1"))
        .unwrap();
    let rows = st
        .query_map(rusqlite::params![match_str], |r| r.get::<_, String>(0))
        .unwrap();
    rows.collect::<Result<Vec<_>, _>>().unwrap()
}

fn arm_a0(m: &MemoryHandle, q: &str) -> (Vec<String>, u128) {
    let t0 = Instant::now();
    let toks = old_query_tokens(q);
    let out = if toks.is_empty() { Vec::new() } else { fts_keys(m.conn(), "rc_shadow", &or_match(&toks)) };
    (out, t0.elapsed().as_micros())
}

fn arm_a1(m: &MemoryHandle, q: &str) -> (Vec<String>, u128) {
    let t0 = Instant::now();
    let out = match rc_index::search(m, q) {
        Ok(hits) => hits.into_iter().map(|h| h.key.key_string()).collect::<Vec<_>>(),
        // 生产口径：纯虚词查询 = 枚举化的 0 行（NoQueryTokens），不是全表扫。
        Err(IndexError::NoQueryTokens) => Vec::new(),
        Err(e) => panic!("A1 臂检索失败：{e}"),
    };
    (out, t0.elapsed().as_micros())
}

fn arm_b1(conn: &Connection, q: &str) -> (Vec<String>, u128) {
    let t0 = Instant::now();
    let out = fts_keys(conn, TRI, &format!("\"{}\"", q.replace('"', "\"\"")));
    (out, t0.elapsed().as_micros())
}

fn arm_b2(conn: &Connection, q: &str) -> (Vec<String>, u128) {
    let t0 = Instant::now();
    // trigram 最短 3 字：2 字词元（中文最常见）永远 0 行，如实让它落 0。
    let toks: Vec<String> =
        query_tokens(q).into_iter().filter(|t| t.chars().count() >= 3).collect();
    let out = if toks.is_empty() { Vec::new() } else { fts_keys(conn, TRI, &or_match(&toks)) };
    (out, t0.elapsed().as_micros())
}

fn build_memory() -> (tempfile::TempDir, MemoryHandle, Vec<(String, String)>) {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::open(dir.path()).unwrap();
    let m = root.into_memory();
    let prov = Provenance { turn_uid: "t".repeat(64), anchor_role: AnchorRole::Agent };
    let mut slugs: Vec<(String, String)> = Vec::new();
    let mut tri_rows: Vec<(String, String)> = Vec::new();
    for (slug, title, body) in ROWS {
        let fc = FactContent {
            kind: "decision".into(),
            title: (*title).into(),
            body: (*body).into(),
            scope: Scope { kind: ScopeKind::Project, key: "proj-g10".into() },
            valid_to_ms: None,
        };
        let k = put_card(&m, &fc, std::slice::from_ref(&prov), NOW).unwrap();
        slugs.push(((*slug).to_string(), k.key_string()));
        // trigram 索引**原文**（title + body），不是影子表的词串——
        // trigram 的价值就是子串匹配，喂词串等于喂第二次分词的结果。
        tri_rows.push((k.key_string(), format!("{title} {body}")));
    }
    rebuild(&m, NOW).unwrap();

    // 旁路实验表：随本测试创建、随 TempDir 消亡，不进生产 schema。
    m.conn()
        .execute_batch(&format!(
            "CREATE VIRTUAL TABLE IF NOT EXISTS {TRI} USING fts5(key UNINDEXED, text, tokenize='trigram');"
        ))
        .unwrap();
    let mut st = m
        .conn()
        .prepare(&format!("INSERT INTO {TRI}(key, text) VALUES(?1, ?2)"))
        .unwrap();
    for (key, text) in &tri_rows {
        st.execute(rusqlite::params![key, text]).unwrap();
    }
    drop(st);
    (dir, m, slugs)
}

#[test]
fn trigram_旁路_ab_四臂对比() {
    let (dir, m, slugs) = build_memory();
    let relevant_of = |p: &Probe| -> Vec<String> {
        p.relevant.iter().map(|s| slugs.iter().find(|(sl, _)| sl == s).unwrap().1.clone()).collect()
    };

    // 暖机：jieba 词典装载（~100ms 级）与语句 prepare 不进计时。
    for p in PROBES {
        let _ = query_tokens(p.q);
        let _ = old_query_tokens(p.q);
    }

    let mut a0v = vec![Arm::default(); PROBES.len()];
    let mut a1v = vec![Arm::default(); PROBES.len()];
    let mut b1v = vec![Arm::default(); PROBES.len()];
    let mut b2v = vec![Arm::default(); PROBES.len()];
    for (i, p) in PROBES.iter().enumerate() {
        let rel = relevant_of(p);
        let (k, us) = arm_a0(&m, p.q);
        a0v[i].record(&k, &rel, us);
        let (k, us) = arm_a1(&m, p.q);
        a1v[i].record(&k, &rel, us);
        let (k, us) = arm_b1(m.conn(), p.q);
        b1v[i].record(&k, &rel, us);
        let (k, us) = arm_b2(m.conn(), p.q);
        b2v[i].record(&k, &rel, us);
    }

    // ===== 程序断言（不靠人工观察）=====
    // 验收 ①：阴性对照组在 A1/B1/B2 全部 0 行；变异证明：A0（旧口径）对「毛姆的小说」必命中，
    // 否则夹具没病、这些 0 恒真。
    for (i, p) in PROBES.iter().enumerate() {
        if p.kind != Kind::Negative {
            continue;
        }
        assert_eq!(a1v[i].rows, 0, "阴性对照「{}」在停用词方案下仍命中", p.q);
        assert_eq!(b1v[i].rows, 0, "阴性对照「{}」在 trigram 短语下仍命中", p.q);
        assert_eq!(b2v[i].rows, 0, "阴性对照「{}」在 trigram 分词下仍命中", p.q);
    }
    let momi = PROBES.iter().position(|p| p.q == "毛姆的小说").unwrap();
    assert!(a0v[momi].rows > 0, "夹具没复现审计里的病（旧口径对阴性查询应命中 ≥1 行）");

    // 验收 ②：真值命中率不降（停用词不许吞真命中）。
    let truth_idx: Vec<usize> =
        PROBES.iter().enumerate().filter(|(_, p)| p.kind == Kind::Truth).map(|(i, _)| i).collect();
    let sum = |v: &[Arm], idx: &[usize]| idx.iter().map(|&i| v[i].true_hits).sum::<usize>();
    let (t0, t1) = (sum(&a0v, &truth_idx), sum(&a1v, &truth_idx));
    assert!(t1 >= t0, "停用词吞了真命中：旧口径 {t0} 新口径 {t1}");

    // trigram 语义 sanity：逐字连读的短语必命中（正控），1-2 字查询永远 0 行（边界）。
    let sched = &slugs.iter().find(|(s, _)| s == "sched_script").unwrap().1;
    let pos = fts_keys(m.conn(), TRI, "\"排期脚本\"");
    assert_eq!(pos.len(), 1, "trigram 正控失败：连读短语应恰好命中排期脚本行");
    assert_eq!(pos[0], sched.as_str(), "trigram 正控命中的行贴错了");
    assert!(fts_keys(m.conn(), TRI, "\"主人\"").is_empty(), "trigram 1-2 字查询必须 0 行（3-gram 边界）");

    // ===== 读数表 =====
    let produced = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    println!("\n===== G10 trigram A/B（四臂，合成夹具） produced_at={produced} arms=A0旧口径/A1停用词/B1tri整句/B2tri分词≥3字 =====");
    println!("{:<24} | {:>11} | {:>11} | {:>11} | {:>11}", "probe(行/真/假)", "A0", "A1", "B1", "B2");
    for (i, p) in PROBES.iter().enumerate() {
        let f = |a: &Arm| format!("{}/{}/{}", a.rows, a.true_hits, a.false_hits);
        println!("{:<24} | {:>11} | {:>11} | {:>11} | {:>11}", p.q, f(&a0v[i]), f(&a1v[i]), f(&b1v[i]), f(&b2v[i]));
    }
    let stat = |name: &str, v: &[Arm]| {
        let (tr, fa, us): (usize, usize, u128) =
            truth_idx.iter().fold((0, 0, 0), |(t, f, u), &i| (t + v[i].true_hits, f + v[i].false_hits, u + v[i].us));
        let neg_rows: usize = PROBES.iter().enumerate().filter(|(_, p)| p.kind == Kind::Negative).map(|(i, _)| v[i].rows).sum();
        let n = truth_idx.len();
        println!(
            "{name}: 真值命中 {tr}/{} 假命中合计 {fa} 阴性组命中 {neg_rows} 查询均耗时 {:.0}µs",
            n,
            us as f64 / n as f64
        );
    };
    stat("A0 旧口径（无停用词）", &a0v);
    stat("A1 停用词（生产口径）", &a1v);
    stat("B1 trigram 整句短语", &b1v);
    stat("B2 trigram 分词≥3字 OR", &b2v);
    println!("（行/真/假 = FTS 命中行数 / 其中真值卡 / 其中噪声卡；A1 走 rc_index::search 全链含事实回读）");

    drop(m);
    drop(dir); // 旁路表随 TempDir 消亡，无残渣
}
