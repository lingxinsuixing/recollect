#[test]
fn 正文不能从载体里被拆出来() {
    trybuild::TestCases::new().compile_fail("tests/compile_fail/*.rs");
}

#[test]
fn fixture_数量有下限() {
    // 与任务 1 同一条判据：删掉的 fixture 不会让任何测试变红，所以必须数它们。
    let n = std::fs::read_dir(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/compile_fail"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "rs").unwrap_or(false))
        .count();
    assert!(n >= 3, "compile_fail fixture 少到 {n} 条，守卫 12 与 `TurnSink` 单方法形状的越界证明已被削薄");
}
