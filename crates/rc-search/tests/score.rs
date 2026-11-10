//! 整数匹配度这一档的判据：四条断言各自钉住公式的一个语义（截断方向、×1024 的量纲、
//! `row_len` 作分母），外加判据 10c——库里不许有分数列。
//! 分数只在查询时算：落列的那一刻它就变成了"重要性"（守卫 7、GOALS §3）。

use rc_memory::{synth, keys::MemKey};
use rc_search::rank::rank;
use rc_search::score::{score_row, FLOOR};
const NOW: i64 = 1_700_000_000_000;

use std::collections::HashSet;

fn q(words: &[&str]) -> HashSet<String> {
    HashSet::from_iter(words.iter().map(|s| s.to_string()))
}

#[test]
fn 命中一个三分之一长度的行等于_341() {
    // 1024 * 1 / 3 = 341.33 → 必须截到 341。截断方向写反（进位）会让长行"刚好过线"。
    let row = vec!["甲".to_string(), "乙".to_string(), "丙".to_string()];
    assert_eq!(score_row(&q(&["甲"]), &row, 3), 341);
}

#[test]
fn 截断方向_2_of_3_必须是_682() {
    // 四舍五入会给出 683。这一条和 341 那条合起来才钉住"向零截断"。
    let row = vec!["甲".to_string(), "乙".to_string(), "丙".to_string()];
    assert_eq!(score_row(&q(&["甲", "乙"]), &row, 3), 682);
}

#[test]
fn 重复查询词不多算_重复行词也不重复计() {
    let row = vec!["甲".to_string(), "甲".to_string(), "乙".to_string()];
    // matched 只数"命中了几个不同的查询词"：{甲} ∩ {甲,乙} = 1。
    assert_eq!(score_row(&q(&["甲", "甲"]), &row, 3), 341);
}

#[test]
// `0 < FLOOR` 看上去是恒真断言（clippy 因此报警），但它**故意**是运行时断言：
// FLOOR 是本阶段唯一"值待实测"的常数（阶段 5 的阈值-召回曲线要改它），
// 这条就是它下界的守卫——证伪 #7（把 FLOOR 改成 0）红的就是这一句。
// 挪进 `const { assert!(…) }` 会把它变成编译期检查，改穿底时不再有用例失败可报。
#[allow(clippy::assertions_on_constants)]
fn 零命中是_0_不是负数也不是_1() {
    let row = vec!["甲".to_string()];
    assert_eq!(score_row(&q(&["乙"]), &row, 1), 0);
    assert_eq!(score_row(&q(&[]), &row, 1), 0);
    assert_eq!(score_row(&q(&["甲"]), &[], 0), 0, "空行不许除零");
    assert!(0 < FLOOR, "零命中必须落在阈值之下");
}

#[test]
fn 边界样本_matched_1_row_len_1024_恰好等于_floor() {
    // 它是 `>=` 与 `>` 的唯一分界。没有这条样本，`above_floor` 的边界语义就是未测状态。
    // 用例名里的 ASCII 段用小写 `floor`：`FLOOR` 会让 `non_snake_case` 报警，
    // 而本仓库的中文用例名一律走小写段（UAX#31 的口径，见既有测试命名）。
    assert_eq!(score_row(&q(&["甲"]), &vec!["甲".to_string(); 1024], 1024), FLOOR);
}

#[test]
fn 超过_1024_个词的长行必须整除到_0() {
    // 这是整数化的**代价**，也是它诚实的地方：长行拿不到分，就沉默。
    // 上一轮这里是浮点，于是"极长行 0.0009 > 阈值 0.0005"能过线——没人能解释那条记忆为什么被提起。
    // 偏离 brief 原文的 2048：两个命中词时 `1024 * 2 / 2048 = 1`，恰好落在 FLOOR 上而不是 0，
    // 那句断言算术不成立。要 0 就得 `row_len > 1024 · matched`，所以这里把行推到 2049——
    // 顺带把"2048 那一行拿 1 分"这个边界本身也钉住了（下面第一句断言）。
    let mut long: Vec<String> = (0..2048).map(|i| format!("词{i}")).collect();
    let mut qq = HashSet::new();
    qq.insert("词0".to_string());
    qq.insert("词1".to_string());
    assert_eq!(score_row(&qq, &long, long.len()), FLOOR, "两个命中词除以 2048 恰好落在 FLOOR 上");
    long.push("词2048".to_string());
    assert_eq!(score_row(&qq, &long, long.len()), 0);
}

#[test]
fn 真实检索路径里长行落在阈值之下() {
    // 用夹具拿一个真实的 0 分，而不是只在纯函数上证它。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let m = root.into_memory();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    // 偏离 brief 原文的 3000：查询"长行低分卡 记录"被切成 5 个词（长/行/低分/卡/记录），
    // 五个命中词对 3004 的词长给的是 `1024*5/3004 = 1`，正好压在线上而不是 0。
    // 要 0 得 `row_len > 1024 · matched`，所以垫到 6000——与 `synth::seed_cells` 里"低分格"那条
    // 用的同一把垫料（那里也写明了"约六千 token"的理由），不另立一个口径。
    synth::seed_padded(&m, now, 6000).unwrap();
    rc_index::rebuild(&m, now).unwrap();
    let got = rank(&m, None, NOW, "长行低分卡 记录").unwrap();
    // R25 的牙：`rank` 末尾那句 `out.sort()` 是"全序"唯一发生在真实检索路径上的地方，
    // 而 `reader::facts` 按 `tbl, id` 回行（内容寻址的 hash 序），删掉 sort 后候选池就退回这个 hash 序。
    // 相对顺序断言（如"原有元素之间不改序"）对这种退回是哑的；只有钉死**绝对位置**才能当场红：
    // 分数最高的一条必须排在第 0 位，那条整除到 0 的垫料长行必须垫在最后。
    assert_eq!(
        got.first().map(|s| s.facts.title.clone()),
        Some("会议记录归档".to_string()),
        "最高分候选没排在首位——`out.sort()` 没生效"
    );
    assert_eq!(
        got.last().map(|s| s.facts.title.clone()),
        Some("长行低分卡".to_string()),
        "0 分长行没垫在末位——`out.sort()` 没生效"
    );
    let padded = got.iter().find(|s| s.facts.title == "长行低分卡").expect("长行没进候选池");
    assert_eq!(padded.score, 0);
    assert!(padded.score < FLOOR, "守卫：长行必须低于阈值");
    let short = got.iter().find(|s| s.score >= FLOOR);
    assert!(short.is_some(), "同一次查询里短行必须过线，否则这条用例什么都没证");
}

#[test]
fn 记忆库里没有任何分数列() {
    // 判据 10c / 守卫 7 的正半：分数只在查询时算，不落列。落列的那一刻它就变成了"重要性"。
    // 裁定 R7：`rc-memory/tests/ownership.rs` 那条归属守卫**不是同一件事**——它数的是列归属表，
    // 这一条数的是"检索层的分数有没有藏进库"，账在任务 5。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let m = root.into_memory();
    let banned = ["score", "similarity", "importance", "weight", "rank", "bm25", "strength"];
    // 自防呆：`MEMORY_TABLES` 若为空，下面的循环整趟空跑、`banned` 一个都没比过却仍然绿——
    // 先钉住清单非空，这条判据才有落点。
    assert!(!rc_memory::schema::MEMORY_TABLES.is_empty(), "MEMORY_TABLES 为空 ⇒ 这条判据是空跑的");
    for table in rc_memory::schema::MEMORY_TABLES {
        let cols: Vec<String> = m
            .conn()
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}') ORDER BY cid"))
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for b in banned {
            assert!(!cols.iter().any(|c| c == b), "{table} 出现分数列 {b}");
        }
    }
}

#[test]
fn 候选键必须在记忆库里找得到_否则响亮报错() {
    // 影子比记忆新 = 索引层有、内容层没有。静默跳过会被读成"这条被否了"（G7）。
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    let m = root.into_memory();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rc_index::rebuild(&m, now).unwrap();
    m.conn()
        .execute(
            // 偏离 brief 原文的 `'幽灵词'`：影子里的 `tokens` 列是**索引侧口径**——空格连接的词串
            // （`analyzer::row_doc` 的输出），而 `query_tokens("幽灵词")` 切出的是 `幽灵` + `词`。
            // 按原文写一整串"幽灵词"，unicode61 把它当成一个词，幽灵行根本进不了候选池，
            // 那条用例就退化成"没有命中 ⇒ Ok(空)"——它要证的 G7 那一半随之失效。
            "INSERT INTO rc_shadow(key, tokens, row_len) VALUES('card:0000000000000000000000000000000000000000000000000000000000000000','幽灵 词',1)",
            [],
        )
        .unwrap();
    // 直接 `expect_err` 会在"本该报错却返回 Ok"时把整份 `Vec<Scored>` 的 Debug（含每条候选的
    // `title`/`body` 正文）吐进测试输出，违反零正文纪律。先把 Ok 侧折成 `len()`，报错面就只剩一个数字。
    let err = rank(&m, None, NOW, "幽灵词")
        .map(|v| v.len())
        .expect_err("幽灵句柄必须报错");
    assert!(matches!(err, rc_search::SearchError::NoFacts(_)), "{err:?}");
}

#[test]
fn 句柄形状可_反解_且反解回同一字符串() {
    // 排序依赖 key_string 的字典序，而字典序要稳定就得先证往返无损。
    let k = MemKey::parse("card:aa5bb6").unwrap();
    assert_eq!(k.key_string(), "card:aa5bb6");
    assert!(MemKey::parse("nonsense:aa5bb6").is_none());
    assert!(MemKey::parse("card:").is_none());
}
