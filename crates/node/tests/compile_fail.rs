#[test]
fn 维护作业在节点的作用域里编译不进来() {
    trybuild::TestCases::new().compile_fail("tests/compile_fail/*.rs");
}

#[test]
fn fixture_数量有下限() {
    let n = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/compile_fail"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "rs").unwrap_or(false))
        .count();
    assert!(n >= 2, "compile_fail fixture 少到 {n} 条：远程维护越界证明被削薄");
}
