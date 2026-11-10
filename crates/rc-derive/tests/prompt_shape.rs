use rc_derive::prompt::{shape, derive_system, derive_user, derive_user_with, derive_user_with_events};
use rc_memory::{keys::{MemKey, MemTable}, RowFacts};

/// 本时点（T10 之后）的 `RowFacts` 是**十七个字段**：T2 加的 `created_at_ms` 之后，
/// T10 又加了 `scope` / `device_id` / `source_agent` 三项（recall 四过滤项的载体）。
/// 本文件是 T10 全仓**三处全字面量夹具**之一，另两处在 `rc-agent` 的 `block_cap.rs`
/// （`full_decision`）与 `recall.rs` 的 `filter_tests::base`（T10 自己新建的那一处）；
/// 漏补就是 E0063，所以这一处把三行写全而不是靠 `..` 兜底。
fn mk_row(id: &str, title: &str, body: &str) -> RowFacts {
    RowFacts {
        key: MemKey { table: MemTable::Card, id: id.into() },
        created_at_ms: 1,
        kind: "decision".into(),
        title: title.into(),
        body: body.into(),
        as_of_ms: 1,
        valid_to_ms: None,
        superseded_by: None,
        retired: false,
        rejected: false,
        verdict: rc_memory::keys::Verdict::Unreviewed,
        attestation: None,
        anchor_role: rc_memory::keys::AnchorRole::Human,
        cooldown_until_ms: None,
        // 任务 10 补的三行（本文件是 T10 全仓两处全字面量夹具之一）。
        scope: rc_memory::Scope { kind: rc_memory::ScopeKind::Project, key: "k".into() },
        device_id: "d".into(),
        source_agent: "a".into(),
    }
}

#[test]
fn shape_is_method_4_standards_4_positives_at_least_1_negatives_at_least_4() {
    let s = shape();
    assert_eq!(s.method_steps, 4, "推导 prompt 必须 4 步（裁定 12）");
    assert_eq!(s.standards, 4, "推导 prompt 必须 4 标准");
    assert!(s.positives >= 1, "至少 1 个正例");
    assert!(s.negatives >= 4, "反例必须 ≥4（架构 §7 推导 prompt 的三段固定四支）");
}

#[test]
fn pair_search_is_anchored_and_shared_entity_is_not_a_relation() {
    let s = derive_system();
    assert!(s.contains("Do not enumerate unrelated pairs"),
            "逐对枚举没有被撤回：M3.1 会在无锚点对上过度关系化");
    assert!(s.contains("shared entity is not a relation"),
            "共享实体被当成了关系依据");
    assert!(!s.contains("enumerate every unordered pair"),
            "旧的逐对枚举指令仍在 prompt 里");
}

#[test]
fn every_negative_is_pinned_in_the_system_prompt() {
    // 反例四支各钉一支**只在反例段出现**的原文短语：钉整句而不是钉"co-occurrence"这类
    // 在 METHOD / STANDARDS 里也出现的散词——散词的断言撤掉反例也照绿（空过的断言不是断言）。
    // 批次 M 段 B 追加第 5 支（mentions 反例），四支原文断言逐字保留。
    let s = derive_system();
    assert!(s.contains("Time-adjacency is co-occurrence, not cause"),
            "反例 1 缺位：时间相邻当因果");
    assert!(s.contains("Same-thing is dedupe, not a relation"),
            "反例 2 缺位：同现实体当关系");
    assert!(s.contains("your own inference"),
            "反例 3 缺位：推测当事实");
    assert!(s.contains("not present in any rendered row"),
            "反例 4 缺位：造新实体");
    assert!(s.contains("not copied verbatim"),
            "反例 5 缺位：mention 名字非逐字");
}

#[test]
fn fourth_key_type_enum_and_chain_shape_are_pinned() {
    // 四键契约的三处新原文：mentions 这个键名、chain type 三值枚举、链至少两环节。
    let s = derive_system();
    assert!(s.contains("mentions"), "第四键 mentions 没进 system prompt");
    assert!(s.contains("causes, enables, prevents"), "chain type 三值枚举缺位");
    assert!(s.contains("at least two rows"), "链的形态（至少两环节）缺位");
    // TAIL 的四键 JSON 示例：键名与 type 的位置逐字钉住（形状漂了模型就会按旧键答）。
    assert!(s.contains("\"mentions\":[{\"row\":\"r1\",\"name\":\"...\"}]"),
            "TAIL 的 mentions 四键示例漂了");
    assert!(s.contains("\"type\":\"causes\""), "TAIL 的 chain type 示例漂了");
    // user 段尾句跟着四键走。
    let user = derive_user(&[mk_row("id-1", "t", "b")]);
    assert!(user.contains("four keys"), "user 段尾句没有改成四键：{user}");
}

#[test]
fn user_segment_does_not_leak_row_ids() {
    // 防走私（与 extract_user "uid 走私"同族）：user 段不许出现具体行 id，只渲染位置别名。
    // 这一支只有行 id 一条断言：`RowFacts` 的任何字段都不是指纹，指纹不在这条数据通路上，
    // 所以"不含 fingerprint 形态 64hex"那种查固定常量的断言永远不会因撤守卫而红（已撤）。
    let rows = vec![mk_row(&"a".repeat(64), "t", "b")];
    let user = derive_user(&rows);
    assert!(!user.contains(&"a".repeat(64)),
            "user 段不应含行 id 全文，应只渲染位置别名 r1");
    // r1 这个 alias 必须出现（位置别名是契约的一部分）
    assert!(user.contains("r1"), "位置别名 r1 必须出现");
}

/// P0-3（2026-10-30）：行钟进 user 段，只用于排序；它不得变成 timeline 的依据日期。
#[test]
fn rows_carry_their_recorded_time_for_ordering_only() {
    let rows = vec![mk_row("id-1", "提交在树上", "验证通过")];
    let user = derive_user(&rows);
    assert!(user.contains("at="), "行钟必须进 ROWS 段：{user}");
    assert!(user.contains("| at |"), "表头必须写出 at 这一格：{user}");
    let system = derive_system();
    assert!(system.contains("only to order rows"),
            "必须写明 at= 只作排序、不作 timeline 依据：{system}");
}

/// P0-3b（2026-10-30）：已知参与者进 who=；缺项渲染 unknown，不猜。
#[test]
fn rows_carry_participants_when_known() {
    let rows = vec![mk_row("id-1", "t", "b")];
    let user = derive_user_with(&rows, &[vec!["张三".into(), "某项目".into()]]);
    assert!(user.contains("who=张三, 某项目"), "参与者必须进 ROWS 段：{user}");
    assert!(user.contains("| who |"), "表头必须写出 who 这一格：{user}");
    let unknown = derive_user(&rows);
    assert!(unknown.contains("who=unknown"), "没有参与者时必须写 unknown：{unknown}");
}

/// 债 #9（修法甲，派发档 I-7 契约 4）：derive 池的 `at=` 同口径——`time_json` 事件时间
/// 在场 ⇒ 用事件钟；缺席 ⇒ 回落 `as_of_ms`（现状链一字不动）。
#[test]
fn at_prefers_the_time_json_event_time_and_falls_back_to_as_of() {
    let rows = vec![mk_row("id-1", "t", "b"), mk_row("id-2", "t2", "b2")];
    // 有事件时间的行：at= 用事件钟（`local_ms_label` 是 ISO 戳，逐字钉事件行那一格）。
    let with = derive_user_with_events(&rows, &[], &[Some(1_700_000_000_000), None]);
    assert!(with.contains("at=2023-11-15T06:13+08:00"),
        "事件时间在场 ⇒ at= 用事件钟：{with}");
    // 无事件时间的行：回落 as_of_ms=1（既有形状，`derive_user` 的对照由它担保）。
    let plain = derive_user(&rows);
    assert_eq!(with,
               plain.replacen("at=1970-01-01T08:00+08:00", "at=2023-11-15T06:13+08:00", 1),
        "回落链必须与 derive_user 的现状逐字节同形（只换事件行的那一格）：\n{with}\n---\n{plain}");
}
