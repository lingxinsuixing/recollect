use std::fs;

/// 守卫 1 的源文本层：rc-derive 的 src 与 Cargo.toml 都不该有"会话/账句柄"的入口。
/// 这一层是"把 `LedgerHandle`/`TranscriptHandle` 写进 src 或 Cargo.toml"那一类越界
/// 的早红点；`use rc_ledger::...` 编译期也挡，但文本断言比编译错误更早一秒（且独立于版本），
/// 而"从 `rc_store::Root` 绕道取柄"那一条**只有这一层挡得住**（见测试 2 的后三支 needle）。
#[test]
fn cargo_toml_does_not_depend_on_rc_ledger() {
    let text = fs::read_to_string("Cargo.toml").unwrap();
    // TOML 的 `#` 行引不住依赖（注释掉的 dep 不是 dep），与测试 2 跳过 `//` 行同口径：
    // 否则一句 `# 无 rc-ledger（守卫 1）` 的说明就把构建弄红，而它没有任何越界。
    let code: Vec<&str> = text.lines().filter(|l| !l.trim_start().starts_with('#')).collect();
    assert!(!code.iter().any(|l| l.contains("rc-ledger")),
            "rc-derive/Cargo.toml 不应依赖 rc-ledger（守卫 1：拿不到会话/账句柄）\n实际内容：\n{text}");
}

#[test]
fn src_does_not_name_ledger_or_transcript_handles() {
    // 走 read_dir 查 src/*.rs（一层，不递归）：禁止在**代码行**出现下面六支 needle。
    // 注释行跳过：lib.rs 的守卫 1 文档必须点名这些禁符才讲得清"为什么"，
    // 整文件子串扫描会把文档自己判死（本机实测：字面版对干净骨架报 1 failed）。
    // 注释引不住任何依赖，`use rc_ledger::...` 这类代码行照旧在扫描面里。
    // 后三支是**绕道**：`Root::open(..).ledger()` 一个禁符都不出现，却把会话/账句柄递进来了——
    // rc-store 是公开依赖，`Root`/`ledger()`/`transcript()` 都 `pub`，所以类型层拦不住这一条，
    // 只能由文本层拦。三支 needle 与守卫 1 的意图一一对应，不是符号洁癖。
    let mut bad: Vec<String> = Vec::new();
    for entry in fs::read_dir("src").unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") { continue; }
        let text = fs::read_to_string(&path).unwrap();
        let code: Vec<&str> = text.lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        for needle in ["LedgerHandle", "TranscriptHandle", "rc_ledger",
                       "Root::open", ".ledger()", ".transcript()"] {
            if code.iter().any(|l| l.contains(needle)) {
                bad.push(format!("{} 含禁用符号 {needle}", path.display()));
            }
        }
    }
    assert!(bad.is_empty(), "rc-derive/src 引用了被禁的句柄符号：\n{}", bad.join("\n"));
}
