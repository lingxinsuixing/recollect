//! 抽取**输入侧**的围栏剪除（§7：只有这一侧有剪刀，推导侧不加第二把）。
//! grammar 的三条定案见计划 §2.10；这里逐条落，且每条都有对应用例（tests/fence.rs）。
//! 判据取向：宁可漏蒸不可闭环——不确定的时候剪，不猜哪一段是人话。

use rc_agent::surface::FENCE;
use rc_contract::normalize_body;
use rc_search::gate::MAX_FORM_LINES;

/// §2.10 第一条：一条 turn 里合法块的数量上限。
pub const MAX_BLOCKS_PER_TURN: usize = 1;

/// 零宽字符只在这一层剥，**绝不写回身份计算**（坑 P-21）。
pub fn strip_zero_width(s: &str) -> String {
    s.chars().filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}')).collect()
}

/// 证据比对视图：`prepare` 之后**只**抹掉行内标记（成对的 `**` 与反引号）。
/// 专供"引文/value 是否是正文的逐字子串"这类判定；落库、身份计算、渲染一律仍用
/// [`prepare`]，两者不许互换。
///
/// 为什么要有第二把尺子：10-29 审计里 4/4 的 `quote_not_verbatim` 只差 `**`/`` ` ``——
/// 模型系统性剥掉行内标记再引用。那不是编造引文，是可归一化的表征差异；拿原标记去比，
/// 等于把"引用规范"判成"引用伪造"。
///
/// 只抹这两类：单 `*`（glob/乘法）与 `_`（文件名）是真实内容，抹了会造出假匹配；
/// 删除后仍是"连续一段"，插入/换序依旧判不逐字。
pub fn evidence_view(s: &str) -> String {
    let prepared = prepare(s);
    let chars: Vec<char> = prepared.chars().collect();
    let mut out = String::with_capacity(prepared.len());
    let mut i = 0;
    while i < chars.len() {
        // 行内链接 `[文字](目标)`：只保留文字。10-30 新语料实测：36 条草稿里 2 条
        // 只差这一层（模型把 `[MEMORY.md](file:///…)` 写成 `MEMORY.md`）——与 `**`/
        // 反引号同族的格式差异，不该判成引文伪造。
        if chars[i] == '[' {
            if let Some(close) = matching(&chars, i, '[', ']') {
                if close + 1 < chars.len() && chars[close + 1] == '(' {
                    if let Some(paren) = matching(&chars, close + 1, '(', ')') {
                        out.extend(chars[i + 1..close].iter());
                        i = paren + 1;
                        continue;
                    }
                }
            }
        }
        if chars[i] == '`' {
            i += 1;
            continue;
        }
        // 链接只删目标时，方括号本身也是格式：两侧都抹掉，`[X]` 与 `X` 视为同一串。
        if chars[i] == '[' || chars[i] == ']' {
            i += 1;
            continue;
        }
        if chars[i] == '*' && i + 1 < chars.len() && chars[i + 1] == '*' {
            while i < chars.len() && chars[i] == '*' {
                i += 1;
            }
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 从 `open_at` 处的开括号起找配对的闭括号（支持嵌套）；找不到返回 `None`。
fn matching(chars: &[char], open_at: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, c) in chars[open_at..].iter().enumerate() {
        if *c == open {
            depth += 1;
        } else if *c == close {
            depth -= 1;
            if depth == 0 {
                return Some(open_at + offset);
            }
        }
    }
    None
}

pub fn prepare(body: &str) -> String {
    let cleaned = strip_zero_width(body);
    let lines: Vec<&str> = cleaned.split('\n').collect();
    let mut keep: Vec<&str> = Vec::new();
    let mut blocks = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        if !lines[i].starts_with(FENCE) {
            keep.push(lines[i]);
            i += 1;
            continue;
        }
        // 数这一串连续前缀行有多长。
        let start = i;
        while i < lines.len() && lines[i].starts_with(FENCE) {
            i += 1;
        }
        let run = i - start;
        let closed = i < lines.len();               // 块后还有行 = 闭合
        blocks += 1;
        let trustworthy = blocks <= MAX_BLOCKS_PER_TURN && run <= MAX_FORM_LINES;
        if !trustworthy {
            // 伪造 ⇒ 从块首行剪到结尾（宁漏不闭环）。这一行是这一层的全部判据。
            break;
        }
        if !closed {
            break;                                   // 未闭合 ⇒ 同样剪到结尾
        }
        // 合法且闭合的块：整块丢掉，块之后的行继续留。
    }
    normalize_body(&keep.join("\n"))
}

#[cfg(test)]
mod evidence_view_tests {
    use super::evidence_view;

    #[test]
    fn inline_marks_do_not_break_verbatim() {
        // 10-29 审计：4/4 的 `quote_not_verbatim` 只差 `**`/反引号。
        let body = "**照绿**，`hub` 已重建";
        assert!(evidence_view(body).contains(&evidence_view("照绿，hub 已重建")));
        assert!(evidence_view(body).contains(&evidence_view("**照绿**，`hub` 已重建")));
        assert!(evidence_view(body).contains(&evidence_view("照绿")));
    }

    #[test]
    fn markdown_link_targets_do_not_break_verbatim() {
        // 10-30 新语料：模型会把 `[MEMORY.md](file:///D:/x.md)` 写成 `MEMORY.md`。
        let body = "更新了，[MEMORY.md](file:///D:/example/a.md) 现在 69 行";
        assert!(evidence_view(body).contains(&evidence_view("更新了，MEMORY.md 现在 69 行")));
        // 模型两种写法都可能：保留方括号、或只写文字。
        assert!(evidence_view(body).contains(&evidence_view("[MEMORY.md] 现在 69 行")));
        assert!(evidence_view(body).contains(&evidence_view("MEMORY.md 现在 69 行")));
        // 去掉链接不该让无关的字也命中。
        assert!(!evidence_view(body).contains(&evidence_view("更新了，other.md 现在 69 行")));
    }

    #[test]
    fn normalization_never_joins_across_insertions_or_reordering() {
        let body = "**照绿**，`hub` 已重建";
        // 插入原文没有的字，仍然不逐字。
        assert!(!evidence_view(body).contains(&evidence_view("照绿，hub 刚刚已重建")));
        // 换序，仍然不逐字。
        assert!(!evidence_view(body).contains(&evidence_view("hub 已重建，照绿")));
    }

    #[test]
    fn single_star_and_underscore_stay_content() {
        // 单 `*`（glob）与 `_`（文件名）是真实内容：抹掉它们会造出假匹配。
        let body = "改了 src/*.rs 与 a_b.rs";
        assert!(!evidence_view(body).contains(&evidence_view("srcrs")));
        assert!(!evidence_view(body).contains(&evidence_view("ab.rs")));
        assert!(evidence_view(body).contains(&evidence_view("src/*.rs")));
    }
}
