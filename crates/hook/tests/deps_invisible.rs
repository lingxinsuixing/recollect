use std::path::Path;
use std::process::Command;

#[test]
fn 三张fixture都在() {
    // fixture 悄悄消失 = 一条从没红过的守卫（任务 7 证伪 4、任务 8 的 fixture 数下限同族）。
    let n = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/compile_fail"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "rs").unwrap_or(false))
        .count();
    assert!(n >= 3, "compile_fail fixture 少于 3 张，实到 {n}");
}

#[test]
fn ledger_llm_net_在hook的作用域里编译不进来() {
    // `.stderr` 一律从**实跑 trybuild 的输出**逐字粘，不许凭记忆写：
    // 具体码是 E0432 还是 E0463 由 rustc 决定，而本判据断的是"编不过"，不是 rustc 的实现细节。
    trybuild::TestCases::new().compile_fail("tests/compile_fail/*.rs");
}

#[test]
fn 依赖树里没有水位会话与网络与diag() {
    let manifest = env!("CARGO_MANIFEST_DIR");
    let ws = Path::new(manifest).parent().unwrap().parent().unwrap();
    let out = Command::new(env!("CARGO"))
        .args(["tree", "-p", "hook", "--prefix", "none", "-e", "normal"])
        .current_dir(ws)
        .output()
        .expect("cargo tree");
    let s = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "cargo tree 失败：{}", String::from_utf8_lossy(&out.stderr));
    for banned in ["rc-ledger", "rc-llm", "rc-net", "rc-diag"] {
        assert!(!s.split_whitespace().any(|w| w == banned), "hook 依赖了 {banned}:\n{s}");
    }
}

#[test]
fn 源码不点名字段也不点水位层() {
    // 守卫 12 在 hook 侧的那一半：`hook` 能拿到 `RowFacts`（它是 rc-search 公开类型链的一环），
    // 但**不许点它的字段**。类型层拦不住"读一个 pub 字段再 println"，所以这一条只能落在源码上。
    // 只看代码不看注释：注释里允许**讨论**这些名字（V13 就得写出来），代码里不许出现。
    let src = include_str!("../src/main.rs");
    let code: String = src
        .lines()
        .map(|l| l.split("//").next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    // R79 的正控（m-3）：上面那行 `split("//")` 会把**行内注释**整段削掉，扫描面因此可能
    // 被削成空/首行而**静默失明**（禁词循环在空 `code` 上永远绿）。这条断言防的就是那一族：
    // `bump_cooldown` 在 `src/main.rs` 的两处非注释代码里（`use` 行与 `stamp_cooldown` 的调用），
    // 正常削完必然还看得见它；它不是禁词，所以永远不会"为了绿"而挡路。
    // 如实登记的残余风险：这条**不防**"同一行的字符串字面量里含 `//`"那一格（评审员实测
    // `const _LEAK: &str = "//RowFacts";` 仍 4 条全绿）——那个形状要同真调用 precede 才造得出，
    // 惯用 Rust 里造不出来，不在本轮夸成已覆盖。
    assert!(code.contains("bump_cooldown"), "行截断把代码削没了，这条判据已经瞎");
    for banned in [
        "RowFacts", "facts.", ".body", ".title", "rc_ledger", "rc_llm", "rc_net", "counters",
    ] {
        assert!(!code.contains(banned), "hook 代码里出现了 {banned}：守卫 6/12 越界");
    }
    // 本文件禁 `/* */` 块注释，否则上面的行截断形同虚设。这条是给下一个写注释的人的。
    assert!(!src.contains("/*"), "不许块注释：源码 grep 判据靠行尾截断");
}
