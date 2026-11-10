//! 词面这一层唯一的尺子：词典、归一、索引侧与查询侧各一个入口。
//! 分词只在这里发生——影子表存的就是它的输出，`rc-search` 拿到词就不再切第二遍。

use std::sync::OnceLock;

/// 词典是进程内唯一的重量级对象：`Jieba::new()` 装载要 100ms 级并分配数 MB。
/// 每次查询重建它，症状是"hook 超预算"；每次重建换一个词典版本，症状是"同一句查询两次结果不同"。
static JIEBA: OnceLock<jieba_rs::Jieba> = OnceLock::new();

fn jieba() -> &'static jieba_rs::Jieba {
    JIEBA.get_or_init(jieba_rs::Jieba::new)
}

/// 只暴露地址，不暴露 `&'static Jieba`：把 jieba 类型放进公开签名等于让每个调用方都能换词典，
/// 而词典必须是这个 crate 的实现细节。测试要证的只有"是不是同一个对象"。
pub fn dict_addr() -> usize {
    jieba() as *const jieba_rs::Jieba as usize
}

/// G10 查询侧停用词小表（来源：2026-11-07 召回审计的假命中证据，不贪多、每词有出处）。
///
/// **只过滤查询侧**：索引侧（[`row_doc`]）一个字都不动，影子表内容不变 ⇒ `NORM_VERSION`
/// **不 +1**、生产库不需要重建（`capability.rs` 里"改 analyzer 必须 +1"针对的是索引侧词面
/// 变化——那才会让旧影子与查询尺子错位；查询侧过滤只少送几个词进 MATCH，旧影子完全适用）。
///
/// 收词出处：
/// - 阴性对照实证：「毛姆的小说」靠虚词「的」假命中 20 行（审计探针 16）⇒ 的；
/// - 16 探针查询串里反复出现的问句框架词（探针 1/3/5/7/9/13/14 的噪声源）⇒ 是/了/什么/谁/为什么/怎么/还；
/// - 任务单给定的常项虚词 ⇒ 在/有/和/就/不/我/你/他/她/它/这/那/也/都/要/会/能、吗/呢。
///   这些词只在 jieba 切不出复合词时才以单词身份出现（会议/纪要这类词在词典里整体切出），
///   能被它们命中的本来就是噪声；「的」也可能当关键词用的那天走引号逃逸口（见 [`quoted_spans`]）。
static STOPWORDS: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();

fn stopwords() -> &'static std::collections::HashSet<&'static str> {
    STOPWORDS.get_or_init(|| {
        [
            "的", "了", "是", "在", "有", "和", "就", "不", "我", "你", "他", "她", "它", "这", "那",
            "也", "都", "要", "会", "能", "还", "什么", "怎么", "谁", "为什么", "吗", "呢",
        ]
        .into_iter()
        .collect()
    })
}

/// 一个（已归一的）词是否在查询侧停用词表里。公开给测试与诊断面读数用，
/// 表本身保持私有（换表属于本 crate 的实现细节，同 [`dict_addr`] 的裁法）。
pub fn is_stopword(token: &str) -> bool {
    stopwords().contains(token)
}

/// 引号逃逸口：查询文本里被 ASCII 双引号成对包住的**字节区间**。
/// 落在区间里的词不进停用词过滤——虚词在 99% 的查询里是噪声，但"停用词不全滤"的
/// 逃逸口必须有：`"的"` 就把「的」当关键词查。引号不成对时按无引号处理（保守侧：
/// 宁可多滤一个虚词，不可让一个漏引号把过滤整个关掉）。
fn quoted_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut open: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c == '"' {
            match open.take() {
                None => open = Some(i + c.len_utf8()),
                Some(s) => spans.push((s, i)),
            }
        }
    }
    spans
}

/// 唯一的词面归一尺子：去空白 + Unicode 小写。索引侧与查询侧都走它，
/// 因为两侧各写一遍就是"我这边匹配、那边不匹配"那一类不可复现的根。
fn norm_token(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn cut(text: &str) -> Vec<String> {
    jieba()
        .cut(text, false)
        .into_iter()
        // jieba-rs 0.11 的 `cut` 返回 `Token`，词面在 `.word`（任务 0 的 `capability::cjk_tokens` 已踩过）。
        .map(|t| norm_token(t.word))
        // 只含标点/符号的"词"不是词：计划的实现只滤空串，于是 `query_tokens(" ！！！ ")`
        // 会切出一个 `！！！` 令牌，把它自己的 `空查询词不许伪装成无记忆`（守卫 8 的另一半：
        // "没法匹配"必须是 `NoQueryTokens`，不许是 `Ok(空)`）挤成假绿。
        // 补在 `cut` 而不是 `query_tokens`：索引侧与查询侧仍然共用这一把尺子（那是 `cjk.rs`
        // 第三条断言的内容），只是这把尺子量的是"有没有词"。
        .filter(|s: &String| !s.is_empty() && s.chars().any(|c| c.is_alphanumeric()))
        .collect()
}

/// 查询侧：去重（匹配度按"命中了几个不同的查询词"算，重复词不该多算）
/// + 停用词过滤（G10，只滤这一侧）+ 引号逃逸口。
///
/// **虚词全被滤空的形状是定义过的**：`query_tokens` 返回空 ⇒ `search` 报
/// `NoQueryTokens`（"query yields no index tokens"）⇒ 消费面（rc-agent 的
/// `query_has_no_tokens`、rc-cycle 的 "index query yields no tokens"）把它当
/// 枚举化的沉默——**返回 0 行，绝不退化成全表扫**。这与守卫 8 同形：空集
/// 不许伪装成"没查到"，必须报"没法匹配"。
///
/// 这里不复用 [`cut`]：保护判定要 jieba `Token` 的字节区间（`start`/`end`），
/// 而 `cut` 只交回词面。标点/空词的过滤与 `cut` 同一条尺子（注释在 `cut` 里）。
pub fn query_tokens(text: &str) -> Vec<String> {
    let spans = quoted_spans(text);
    let mut seen = std::collections::HashSet::new();
    jieba()
        .cut(text, false)
        .into_iter()
        .filter_map(|t| {
            let tok = norm_token(t.word);
            // 同 `cut` 的守卫：空串与纯标点不是词，不许挤掉 `NoQueryTokens` 的形状。
            if tok.is_empty() || !tok.chars().any(|c| c.is_alphanumeric()) {
                return None;
            }
            // 词面与任一引号区间重叠 ⇒ 受保护，停用词表不适用。
            // （引号本身是标点，切出来的引号"词"已在上一支被滤，不会泄漏进 MATCH。）
            let protected = spans.iter().any(|&(s, e)| t.start < e && s < t.end);
            if !protected && is_stopword(&tok) {
                return None;
            }
            seen.insert(tok.clone()).then_some(tok)
        })
        .collect()
}

/// 索引侧：**不去重**——`row_len` 是长度归一的分母，去重会让长行看起来短。
/// 返回 `(空格连接的词串, 词数)`；空格由 `norm_token` 保证不会是词面的一部分。
pub fn row_doc(title: &str, body: &str) -> (String, usize) {
    let mut toks = cut(title);
    toks.extend(cut(body));
    let len = toks.len();
    (toks.join(" "), len)
}
