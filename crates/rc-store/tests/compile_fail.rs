#[test]
fn out_of_bounds_uses_do_not_compile() {
    // 每条 "compile_fail/*.rs 必须编译失败" 都是一个守卫的牙（架构 §3 自检计数）
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/*.rs");
}

#[test]
fn compile_fail_fixture_roster_does_not_shrink() {
    // fixture 悄悄消失 = 守卫悄悄消失。牙的条数要有下限断言，否则删文件不会让任何测试变红。
    let n = std::fs::read_dir("tests/compile_fail")
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "rs").unwrap_or(false))
        .count();
    assert!(n >= 4, "compile_fail 目录只剩 {n} 条 .rs fixture，下限是 4（阶段 0 两条 + 任务 1 两条）");
}
