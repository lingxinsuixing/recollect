use rc_contract::normalize_body;

#[test]
fn crlf_becomes_lf() {
    assert_eq!(normalize_body("a\r\nb"), "a\nb");
}

#[test]
fn leading_and_trailing_whitespace_is_stripped() {
    assert_eq!(normalize_body("  \n 中文 \t "), "中文");
}

#[test]
fn internal_spaces_are_preserved() {
    // 架构 §5：内部空格保留——它可能是缩进代码或有意的分隔
    assert_eq!(normalize_body("a  b   c"), "a  b   c");
}

#[test]
fn nfc_is_applied() {
    // U+0065 U+0301 → U+00E9
    assert_eq!(normalize_body("e\u{0301}"), "\u{00e9}");
}

#[test]
fn normalization_is_idempotent() {
    let once = normalize_body("a\r\n  é ");
    assert_eq!(normalize_body(&once), once);
}
