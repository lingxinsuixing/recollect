//! A39：六词是封闭词表；顺序与字面量都钉死，任何"顺手加一种"都会红。

#[test]
fn entity_kinds_是_a39_恢复的六词且顺序固定() {
    assert_eq!(
        rc_contract::ENTITY_KINDS,
        ["person", "project", "org", "tool", "place", "concept"]
    );
}

#[test]
fn is_valid_entity_kind_是精确匹配_归一只住在抽取闸门() {
    for k in rc_contract::ENTITY_KINDS {
        assert!(rc_contract::is_valid_entity_kind(k), "{k} 应在词表内");
    }
    assert!(!rc_contract::is_valid_entity_kind("mood"));
    assert!(!rc_contract::is_valid_entity_kind("human"));
    assert!(!rc_contract::is_valid_entity_kind("causal"));
    assert!(!rc_contract::is_valid_entity_kind("timeline"));
    assert!(!rc_contract::is_valid_entity_kind("Person"), "契约侧不做大小写归一");
}
