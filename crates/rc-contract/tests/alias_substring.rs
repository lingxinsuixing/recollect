//! A41 判据的形状：正反两半各一条，各自能因移除 `contains` 或移除"空串 ⇒ false"而红。

#[test]
fn alias_是_source_子串_判真() {
    let src = "Alice said the launch is next week.";
    assert!(rc_contract::is_alias_in_source("Alice", src));
    assert!(rc_contract::is_alias_in_source("launch", src));
    // 子串可以是任意片段——判据只看字节 in，不看词元。
    assert!(rc_contract::is_alias_in_source("next we", src));
}

#[test]
fn alias_不在_source_或_空_判假() {
    let src = "Alice said the launch is next week.";
    assert!(!rc_contract::is_alias_in_source("Bob", src),
        "别名不在原文 ⇒ 拒收");
    assert!(!rc_contract::is_alias_in_source("", src),
        "空别名不算别名（守卫 10 的 pronoun 闸门另一支）");
    // 反向：源是空串，任何非空别名都不可能在里面
    assert!(!rc_contract::is_alias_in_source("Alice", ""));
}
