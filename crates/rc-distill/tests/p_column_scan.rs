//! 整词扫描守卫（§7："schema 里不许出现 P 独占列名，这条有整词扫描守卫，扫描集由归属表生成，不手抄"）。

use rc_memory::{Actor, OWNERSHIP};

/// 整词命中：前后都不是 [A-Za-z0-9_]。`source_agent` 出现在 `source_agent_id` 里不算命中，
/// 因为那是另一个标识符——**判据是标识符不是子串**，否则守卫会被自己的宽松命中废掉。
fn names_whole_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut from = 0usize;
    while let Some(i) = haystack[from..].find(needle) {
        let start = from + i;
        let end = start + needle.len();
        let word = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
        // 边界判据用的是**字节**而不是 char：ASCII 标识符里出现的分隔点只可能是单字节，
        // 而中文字符的 UTF-8 三字节序列每字节都 >= 0x80，`word()` 对它返回 false ⇒
        // "source_agent进度"里的 `source_agent` 判为整词命中（这正是我们要的红），不会误判为"半个词"。
        let left_ok = start == 0 || !word(bytes[start - 1]);
        let right_ok = end >= bytes.len() || !word(bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        from = end;                              // 只往前走 needle.len()：重叠命中不会漏
    }
    false
}

fn program_owned() -> Vec<String> {
    let mut v: Vec<String> = OWNERSHIP.iter().filter(|o| o.actor == Actor::Program)
        .map(|o| o.column.to_string()).collect();
    v.sort();
    v.dedup();
    v
}

#[test]
fn scan_set_is_derived_from_the_ownership_table_and_is_not_empty() {
    let set = program_owned();
    // 坑 P-08：空扫描集 = 空转守卫。这条断的是"守卫有牙"，不是"代码跑过了"。
    assert!(set.len() >= 8, "扫描集只有 {} 项 ⇒ 归属表读不到东西", set.len());
    for must in ["source_agent", "device_id", "created_at_ms", "norm_version", "as_of_ms", "cooldown_until_ms"] {
        assert!(set.iter().any(|c| c == must), "扫描集里没有 {must}（归属表改了？还是这条守卫本来就没牙）");
    }
}

#[test]
fn the_scan_helper_has_a_positive_control() {
    // P-08 的镜像面：扫描集非空只保证"有待扫的名字"，不保证 helper 扫得到。
    // 一个恒 false 的 `names_whole_word` 会让下面那条否定断言永绿——守卫照样空转，
    // 而这一次没有任何东西兜住它。所以命中能力必须自己有一条正面断言，
    // 并且把上面注释承诺的两类边界各钉一次（中文紧跟 = 命中、半个标识符 = 不命中）。
    assert!(names_whole_word(r#"cards: [{"source_agent": x}]"#, "source_agent"), "整词命中必须为真");
    assert!(names_whole_word("source_agent进度", "source_agent"), "紧跟中文字符算整词命中");
    assert!(!names_whole_word("source_agent_id", "source_agent"), "半个标识符不算命中");
    assert!(!names_whole_word("provenance_source_agent", "source_agent"), "左邻下划线不算命中");
}

#[test]
fn the_output_contract_names_no_p_owned_column() {
    let schema = rc_distill::contract::OUTPUT_CONTRACT_SCHEMA;
    for name in program_owned() {
        assert!(!names_whole_word(schema, &name), "输出契约里出现了 P 独占列名 {name} ⇒ X 被要求填 P 的列");
    }
}

#[test]
fn the_contract_does_name_the_provenance_key_once() {
    // 反面：扫描集不许把该说的也扫掉。provenance 是 X 必带的东西（§2 规则 2），它必须在契约文本里出现。
    assert!(rc_distill::contract::OUTPUT_CONTRACT_SCHEMA.contains("provenance"));
}
