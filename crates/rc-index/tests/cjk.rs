use rc_index::analyzer::{dict_addr, query_tokens, row_doc};

#[test]
fn 中文查询必须切得出词_不许返回空集() {
    // 守卫 8。空集 = probe 永远沉默 = 用户读成"系统没有记忆"。
    let toks = query_tokens("上周老王把部署脚本改成了 bash");
    assert!(!toks.is_empty(), "守卫 8：中文分词返回空集");
    assert!(toks.contains(&"部署".to_string()), "应切出「部署」，实际 {toks:?}");
    assert!(toks.contains(&"脚本".to_string()), "应切出「脚本」，实际 {toks:?}");
}

#[test]
fn 词是词不是整行() {
    let toks = query_tokens("记忆 系统 需要 分层");
    assert!(toks.len() >= 4, "整行当一个词 ⇒ FTS 永不命中，{toks:?}");
    assert!(toks.iter().all(|t| !t.is_empty() && !t.contains(' ')));
}

#[test]
fn 索引侧与查询侧共用同一把归一尺子() {
    // 两侧各写一遍 to_lowercase 就是下一次漂移的入口：这条断言让"尺子不同"当场红。
    let (doc, _len) = row_doc("DevOps 部署", "把 CI 接进了 Bash");
    let q = query_tokens("devops 部署");
    assert!(q.iter().all(|t| doc.split(' ').any(|d| d == *t)), "同一词面两侧形状不同：doc={doc} q={q:?}");
    assert!(doc.split(' ').all(|t| !t.is_empty() && !t.contains(' ')));
}

#[test]
fn 词典在进程内只有一份() {
    assert_eq!(dict_addr(), dict_addr(), "每次调用重建 Jieba ⇒ 每轮多 100ms 级开销");
}
