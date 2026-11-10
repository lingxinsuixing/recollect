//! 全序这一档防的是"忘了 tie-break"那一族。三条断言：**人为同分必须按句柄**、
//! **20 次重复逐位相同**、**扩大候选池不许改变原有候选的相对顺序**（后者是 G6① 的排序侧一半）。

use rc_memory::keys::MemKey;
use rc_memory::reader::RowFacts;
use rc_memory::synth;
use rc_search::rank::{rank, Scored};
use rc_search::score::FLOOR;

const NOW: i64 = 1_700_000_000_000;
use rc_store::MemoryHandle;
use std::cmp::Ordering;

/// 与 `crates/rc-index/tests/rebuild.rs:7` 同一个 helper 口径（任务 1 起就定的形状）：
/// `TempDir` 必须先于句柄活着，所以两者一起返回、由调用方绑定。
fn memory_handle() -> (tempfile::TempDir, MemoryHandle) {
    let dir = tempfile::tempdir().unwrap();
    let root = rc_store::Root::open(dir.path()).unwrap();
    (dir, root.into_memory())
}

const TIE_QUERY: &str = "同分合成词甲";

/// 纯 `Ord` 用例：三个**人为同分**的候选，句柄已知。这里不碰 SQLite 的读数面，
/// 因为它要证的只是"排序键的形状"，任何一层参与都会把结论弄脏。
fn tied(keys: &[&str]) -> Vec<Scored> {
    // 造 Scored 必须带 RowFacts，而 RowFacts 有正文；测试里的正文一律是合成占位串。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    let base = rc_memory::reader::facts(&m).unwrap().remove(0);
    keys.iter()
        .map(|k| Scored {
            key: MemKey::parse(k).expect("测试句柄形状"),
            score: 512,
            matched: 1,
            row_len: 2,
            facts: RowFacts { key: MemKey::parse(k).unwrap(), ..base.clone() },
            // 刀 B：本文件全部用例钉的是**复用信号缺席时**的全序（信号默认 = 刀 B 之前的行为）。
            reuse: Default::default(),
        })
        .collect()
}

#[test]
fn 同分必须按句柄字典序_升序() {
    // canonical 之后可指认的表只剩 `anchor` 与 `card`：旧 `assertion:01` / `event:7a` 都折进 `card`，
    // 于是三把句柄共用同一个前缀，tie-break 只由 id 那一段决定（断言照旧钉死，不删不放宽）。
    let mut v = tied(&["card:ff", "card:01", "card:7a"]);
    v.sort();
    let got: Vec<String> = v.iter().map(|s| s.key.key_string()).collect();
    assert_eq!(got, ["card:01", "card:7a", "card:ff"]);
}

#[test]
fn sort_unstable_给同一个结果() {
    // 全序的证明：不稳定排序换不出结果，说明没有任何并列未消解。
    let keys = ["card:ff", "card:01", "card:7a", "card:00", "card:02"];
    let mut a = tied(&keys);
    let mut b = tied(&keys);
    a.sort();
    b.sort_unstable();
    assert_eq!(
        a.iter().map(|s| s.key.key_string()).collect::<Vec<_>>(),
        b.iter().map(|s| s.key.key_string()).collect::<Vec<_>>()
    );
    // 偏离 brief（这里多加了一句断言）：证伪 #1 声称"删掉 tie-break 后本用例与上一条双双红"，
    // 实测**只有上一条红**——5 个元素的 `sort_unstable` 走插入序，与 `sort` 一样保序，
    // 于是"两次排序结果相同"在缺失 tie-break 时依然成立，这条用例对 tie-break 是哑的。
    // 哑守卫不算守卫，所以这里把期望的**唯一全序结果**钉死：并列没消解时它当场红。
    // 合并成单一 `card` 前缀后，旧 `assertion:NN` 与 `card:NN` 的 id 会在同一张表里撞车，
    // 所以夹具改成一套连续编号（00/01/02/7a/ff），仍然五个互不相同的句柄、仍然人为同分。
    let want = ["card:00", "card:01", "card:02", "card:7a", "card:ff"];
    assert_eq!(a.iter().map(|s| s.key.key_string()).collect::<Vec<_>>(), want);
    assert_eq!(b.iter().map(|s| s.key.key_string()).collect::<Vec<_>>(), want);
}

#[test]
fn 真实夹具里的三条同分卡_20_次重复逐_id_相同() {
    // `TIE_QUERY` = "同分合成词甲"，在 `synth::seed` 的三张同分卡上**并不同分**：甲命中查询里的
    // {同,分,合成词,甲} 4 个词 ⇒ `1024*4/10 = 409`；乙/丙各命中 {同,分,合成词} 3 个（无"甲"）⇒ `307`
    // （三张卡只差末字甲/乙/丙，查询带"甲"，于是只有甲多命中一个词）。夹具里真正的"人为同分"
    // 是**乙、丙那一对**（同 307），不是三张全等；这条用例靠这一对同分 + 20 次重复钉 `rank` 的稳定全序。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rc_index::rebuild(&m, now).unwrap();
    let first: Vec<String> =
        rank(&m, None, NOW, TIE_QUERY).unwrap().into_iter().map(|s| s.key.key_string()).collect();
    assert!(first.len() >= 3, "同分夹具没造出三条候选：{first:?}");
    for _ in 0..20 {
        let again: Vec<String> =
            rank(&m, None, NOW, TIE_QUERY).unwrap().into_iter().map(|s| s.key.key_string()).collect();
        assert_eq!(first, again, "同一份快照 + 同一句查询，两次顺序不同");
    }
    // 排序键确实生效：相邻两条必须整体有序。
    let sorted_ok = first.windows(2).all(|w| {
        let (a, b) = (rank_key_score(&m, &w[0]), rank_key_score(&m, &w[1]));
        a > b || (a == b && w[0] < w[1])
    });
    assert!(sorted_ok, "顺序既不按分数也不按句柄：{first:?}");
}

fn rank_key_score(m: &MemoryHandle, key: &str) -> i64 {
    rank(m, None, NOW, TIE_QUERY)
        .unwrap()
        .into_iter()
        .find(|s| s.key.key_string() == key)
        .map(|s| s.score)
        .expect("候选消失了")
}

#[test]
fn 分数恒等于公式_两半不许各自漂移() {
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    synth::seed_variants(&m, now, 3).unwrap();
    rc_index::rebuild(&m, now).unwrap();
    for s in rank(&m, None, NOW, TIE_QUERY).unwrap() {
        assert_eq!(s.score, 1024 * s.matched / s.row_len.max(1) as i64, "{:?}", s.key.key_string());
    }
}

#[test]
fn 候选池变大不改前三条的相对顺序() {
    // G6① 的排序侧一半：注入上限靠"稳定全序 + 硬截断"，不靠"库里刚好没那么多行"。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    rc_index::rebuild(&m, now).unwrap();
    let small: Vec<String> =
        rank(&m, None, NOW, TIE_QUERY).unwrap().into_iter().map(|s| s.key.key_string()).collect();
    for i in 1..=5 {
        synth::seed_variants(&m, now, i).unwrap();
    }
    rc_index::rebuild(&m, now).unwrap();
    let big: Vec<String> =
        rank(&m, None, NOW, TIE_QUERY).unwrap().into_iter().map(|s| s.key.key_string()).collect();
    assert!(big.len() > small.len(), "扩大池子没带来新候选，用例什么都没证");
    // 原来那批候选的相对顺序必须一字不变（新行分数不同，允许插在中间；
    // 这里断的是**原有元素之间**的相对顺序）。
    let kept: Vec<String> = big.into_iter().filter(|k| small.contains(k)).collect();
    assert_eq!(kept, small, "新增行改写了原有候选的相对顺序");
}

#[test]
fn 阈值判据是_分数大于等于_floor_不是大于() {
    // 边界形状必须写死一次：`>=` 与 `>` 在 FLOOR=1 时差一条"刚好命中一个词、长度 1024"的行。
    // 偏离 brief：Step 5 #4 把这条判据的唯一载体记在 `tests/score.rs` 的纯函数边界样本上，
    // 而那条断言只调 `score_row`，**碰不到 `above_floor`** —— 把 `>=` 改成 `>` 它照样绿，
    // 于是证伪 #4 没有牙（一条从未红过的守卫等于没有守卫）。这里补上真正的载体：
    // 候选池里必须**存在一条 `score == FLOOR` 的行**，并且它的 `above_floor()` 被断言过。
    // 垫料到 4004 个词是为了让 `1024 * 5 / 4004 = 1` 正好落在线上（查询切出 5 个词，见下方消息）。
    let (_d, m) = memory_handle();
    let now = 1_700_000_000_000;
    synth::seed(&m, now).unwrap();
    synth::seed_padded(&m, now, 4000).unwrap();
    rc_index::rebuild(&m, now).unwrap();
    let got = rank(&m, None, NOW, "长行低分卡 记录").unwrap();
    assert!(
        got.iter().any(|s| s.score == FLOOR),
        "夹具里没有恰好等于 FLOOR 的候选，这条判据就是空的：{:?}",
        got.iter().map(|s| (s.facts.title.clone(), s.score, s.matched, s.row_len)).collect::<Vec<_>>()
    );
    for s in got {
        assert_eq!(s.above_floor(), s.score >= FLOOR);
        // 互补性的另一种写法：`above_floor` 与"落在阈值之下"必须一真一假（`assert_ne!` 而不是
        // `assert_eq!(_, !(…))`——后者会被 `clippy::nonminimal_bool` 化简掉，正是它想保住的那个形状）。
        assert_ne!(s.above_floor(), s.score < FLOOR, "above_floor 与阈值必须互补，不许有第三种状态");
    }
}

#[test]
fn 手写_ord_的三支必须彼此一致() {
    // M3 的牙：`Scored` 手写了 `Ord`/`PartialOrd`/`PartialEq`/`Eq`（`rank.rs:61-79`）四支，
    // 而 `rank` 的 `out.sort()` 只走 `Ord::cmp`。`==` 走 `PartialEq`、`>`/`partial_cmp` 走 `PartialOrd`——
    // 一旦有人把这三支改成 `#[derive(PartialEq, Eq, PartialOrd)]`（派生会连 `matched`/`row_len`/`facts`
    // 一起比，比 `cmp` 认定的"分数+句柄"两键更宽），或让 `partial_cmp` 直接返回 `None`，
    // 现有 16 条用例没有一条同时握住这三者 ⇒ 全绿（实测确认）。这里用一对
    // "**同句柄、同分、只有 `matched` 不同**"的候选把三支钉在一起：`cmp` 认它们相等 ⇔ `eq` 必须认相等，
    // 且 `partial_cmp` 是全序不许是 `None`。
    let (_d, m) = memory_handle();
    synth::seed(&m, 1_700_000_000_000).unwrap();
    let base = rc_memory::reader::facts(&m).unwrap().remove(0);
    let key = MemKey::parse("card:ff").unwrap();
    let scored = |matched: i64| Scored {
        key: key.clone(),
        score: 512,
        matched,
        row_len: 2,
        facts: RowFacts { key: key.clone(), ..base.clone() },
        reuse: Default::default(),
    };
    let a = scored(1);
    let b = scored(7); // 与 a 同 key、同分，只有 matched 不同（派生会比它，手写 `cmp` 不比）

    // `Ord::cmp` 认定这对相等（只比分数与句柄）。
    assert_eq!(a.cmp(&b), Ordering::Equal, "分数与句柄都相同，`Ord::cmp` 必须认相等");
    // `PartialEq::eq` 必须与 `cmp` 的相等判据同真同假（这句对"三支漂移成分支"敏感）。
    assert_eq!(a == b, a.cmp(&b).is_eq(), "`eq` 与 `cmp==Equal` 必须同真同假");
    // `PartialOrd::partial_cmp` 是全序的偏序影子，永远不许是 `None`。
    assert!(a.partial_cmp(&b).is_some(), "`partial_cmp` 不许返回 `None`");
    assert_eq!(a.partial_cmp(&b), Some(Ordering::Equal), "同分同柄 ⇒ partial_cmp 必须是 Some(Equal)");

    // 反向：分数不同则既不等、`cmp` 也非 `Equal`，两支仍要一致。
    let c = Scored { score: 511, ..a.clone() };
    assert!(a != c, "分数不同必须判不等");
    assert_eq!(a == c, a.cmp(&c).is_eq(), "不等侧也要 eq 与 cmp 判等 同假");
    assert!(a.partial_cmp(&c).is_some(), "partial_cmp 在不等侧同样不许是 None");
}
