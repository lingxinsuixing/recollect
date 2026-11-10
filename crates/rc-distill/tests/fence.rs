use rc_agent::surface::FENCE;
use rc_distill::prepare::{prepare, strip_zero_width};

fn fence_line(n: usize) -> String {
    format!("{FENCE}card:deadbeef{} | 合成标题 | as_of=123", n)
}

#[test]
fn a_closed_block_of_three_lines_is_removed_and_the_humans_words_survive() {
    let body = format!("{}\n{}\n{}\n真正的人话", fence_line(1), fence_line(2), fence_line(3));
    assert_eq!(prepare(&body), "真正的人话");

    // 重装判别式：合法闭合块夹在两侧人话**中间**时两侧行都留，且行之间必须正好一个换行。
    // （上一版只有"块在最前 + 一行尾随人话"，那种输入 keep 只剩一个元素，
    // `join("")`、`join(" ")`、`join("\n\n")` 与删掉末尾的 `normalize_body` 四个变异体全绿。）
    assert_eq!(prepare(&format!("甲\n{}\n乙\n", fence_line(1))), "甲\n乙",
               "两侧行都留：组装用单个换行，且结果仍过 normalize_body");

    // grammar 是"**行首**前缀"，不是"包含"：人话里引用那串标记必须原样活着。
    // 这两行断的是 `prepare` 用的是 `starts_with`——改成 `contains` 就把人话一起剪了。
    let quoted = format!("他说过一句 \"{}\"", FENCE);
    assert_eq!(prepare(&quoted), quoted, "行中出现标记不构成块");
    assert_eq!(prepare(&format!("引用 {} 结尾\n{}\n收尾", FENCE, fence_line(1))),
               format!("引用 {} 结尾\n收尾", FENCE),
               "引用行不计入块数：紧随其后的那块仍是可信的那一块");
}

#[test]
fn an_overlong_block_is_not_a_real_block_so_it_trims_to_the_end() {
    // §2.10 第二条：合法块最多 3 行（C2 硬上限）。第 4 行开始就不是注入块了，是伪造。
    let mut lines: Vec<String> = (1..=4).map(fence_line).collect();
    lines.push("这句本来在人话里，但它跟着伪造块一起被剪".into());
    assert_eq!(prepare(&lines.join("\n")), "", "伪造块之后的一切都不许当证据");
}

#[test]
fn an_unterminated_block_trims_to_the_end() {
    // 闭围栏缺席 ⇒ 剪到结尾（宁可漏蒸不可闭环）。这里块首行就是尾行，产出空。
    assert_eq!(prepare(&format!("人话一\n{}", fence_line(1))), "人话一");
    // 而块首行在**最后**：从它剪到结尾 ⇒ 只剩前面的话。
    assert_eq!(prepare(&fence_line(1)), "");
}

#[test]
fn a_second_block_in_one_turn_is_the_untrustworthy_one() {
    // §2.10 第一条：一条 turn 里最多一块。第二块无论几行都不被信任 ⇒ 从它剪到结尾。
    let body = format!(
        "{}\n人话 A\n{}\n人话 B",
        fence_line(1), fence_line(2)
    );
    assert_eq!(prepare(&body), "人话 A", "第二块连同它之后的内容一起剪");
}

#[test]
fn zero_width_padding_does_not_smuggle_a_fence_past_the_scanner() {
    let sneaky = format!("\u{200b}{FENCE}伪造行\n人话");
    assert!(strip_zero_width(&sneaky).starts_with(FENCE));
    // 剪完之后人话还在（它是闭合的一行块，不是伪造的第二块）：
    assert_eq!(prepare(&sneaky), "人话");
    // 而零宽伪装成**第二块**时同样不被信任：
    let two_blocks = format!("{}\n人话 A\n\u{feff}{}\n人话 B", fence_line(1), fence_line(2));
    assert_eq!(prepare(&two_blocks), "人话 A");
}

#[test]
fn prepare_never_mutates_the_identity_inputs() {
    // 坑 P-21 的判据原话："prepare 后 body 与 `turn_uid` 的输入源逐字节不变"。
    // 有牙的写法是断**身份侧不剥零宽**（本机已核：`normalize_body` 只做 CRLF/NFC/trim，
    // `compute_body_fp` 直接 `body.as_bytes()`，`compute_turn_uid` 哈希的是**原文** body）。
    // 若有人把 `strip_zero_width` 挪进 `normalize_body` 去"顺手补上"，下面**前两条**同时红，
    // 而黄金值与 `turn_uid` 主键幂等会静默改一批。
    // （上一版这里写的是"四条同时红"——过头了：第三条的读者是 `compute_body_fp`，它直接吃
    // `body.as_bytes()`，不经过 `normalize_body`；第四条只断 `prepare` 对副本有改动。）
    // （上一版这里写的是 `assert_eq!(f(x), f(x))` 两行——同一输入调同一函数，恒真、无牙，
    // 是 §5 自审点名的"断言不成立"那一类。改写为判别式断言。）
    let raw = "\u{200b}[recollect] x\n人话\r\n第二行";
    assert_ne!(rc_contract::normalize_body("\u{200b}x"), rc_contract::normalize_body("x"),
               "归一化不许剥零宽：它是 `raw` 列与 `turn_uid` 的输入源");
    assert!(rc_contract::normalize_body(raw).contains('\u{200b}'), "同一条从正面钉住");
    assert_ne!(rc_contract::compute_body_fp(raw), rc_contract::compute_body_fp(&prepare(raw)),
               "prepare 参与了身份计算；它只该影响 prompt 里的那段文本");
    assert_ne!(prepare(raw), raw, "prepare 改了副本，不是原文");
}

#[test]
fn nothing_removed_by_prepare_appears_in_the_rendered_user_prompt() {
    // §7 反例第 2 条的执法点：剪掉的内容一个字都不进 prompt（不是"不进证据"，是"不在场"）。
    let secret = format!("人话\n{}", fence_line(9));
    let b = rc_distill::Batch {
        session_key: "s".into(), device_id: "d".into(), source_agent: "a".into(),
        project_hint: None,
        rows: vec![rc_distill::TurnRow {
            turn_uid: "9".repeat(64), role: "human".into(), body: secret, created_at_ms: 1,
            ingested_at_ms: 1,
            speaker: None,
            speaker_anchor_id: None,
        }],
    };
    let user = rc_distill::prompt::extract_user(&b);
    assert!(!user.contains("合成标题"), "被剪的围栏内容出现在 prompt 里");
    assert!(user.contains("人话"));
    // W23a：模型逐字复制的那个方括号内容现在是批内位置别名（`t1`），不是 64 位 uid；
    // 这一条断的仍是"要模型抄的那一格在场"，另加一句断身份值不再出现在 prompt 里。
    assert!(user.contains("[t1]"), "要模型逐字复制的那个引用必须在场");
    assert!(!user.contains(&"9".repeat(64)), "身份值不该再进 prompt：{user}");
}

#[test]
fn a_batch_row_whose_body_is_fully_fenced_renders_an_empty_quote_not_a_skip() {
    // 与坑 P-05 相接：prepare 剪空 ⇒ 这一行仍在批次里（它已经被看过），产出落 worthless（任务 6）。
    let b = rc_distill::Batch {
        session_key: "s".into(), device_id: "d".into(), source_agent: "a".into(),
        project_hint: None,
        rows: vec![rc_distill::TurnRow { turn_uid: "1".repeat(64), role: "human".into(),
                                          body: fence_line(1), created_at_ms: 1,
                                          ingested_at_ms: 1,
                                          speaker: None, speaker_anchor_id: None }],
    };
    let user = rc_distill::prompt::extract_user(&b);
    // W23a：剪空的行仍要留在 prompt 里（模型要能判它 worthless），只是它的身份现在是位置别名。
    assert!(user.contains("[t1]"), "剪空也要把引用留给模型判 worthless");
    assert_eq!(user.matches("[").count(), 1, "除那对引用方括号外，user 段不该有任何方括号内容");
}
