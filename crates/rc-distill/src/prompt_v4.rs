//! v4 A1（锚 / 提及 / 别名）的输入渲染。
//!
//! 系统模板本体住在 `prompt.rs`——那里是 canary 常量唯一的家，也是四份作业共用的拼装处。
//! 本文件只负责把一批 turns 与已有候选锚渲染成 user 段。

use crate::batch::{Batch, TurnRow};
use crate::prepare::prepare;
use crate::prompt::is_compact_summary;

pub use crate::prompt::{a1_shape, a1_system};

/// 模型可见的轮次引用：批内序号（t1、t2…），压缩摘要轮不占号。
/// 渲染与解析必须共用这一处，否则 `tN` 会指向不同的行。
pub fn a1_refs(batch: &Batch) -> Vec<(String, &TurnRow)> {
    let mut refs = Vec::with_capacity(batch.rows.len());
    for row in &batch.rows {
        if is_compact_summary(&row.body) {
            continue;
        }
        refs.push((format!("t{}", refs.len() + 1), row));
    }
    refs
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorCandidate {
    pub kind: String,
    pub canonical: String,
    pub aliases: Vec<String>,
}

/// `user` = **这批话是谁说的**（一格）+ 已有候选锚（只用于统一命名）+ 逐轮原文。
/// 候选锚不渲染数据库 id：模型没有引用它们的入口，给了 id 只会诱导它抄进输出。
///
/// H6：候选锚的 `kind`/`canonical` 也过 `prepare`。名字是从用户原话里摘的（canonical 要求
/// 逐字），零宽字符与围栏块可以借这一格绕过正文那把剪刀——同一个输入面的每个字一律同剪。
///
/// P0-1：第一行 `SESSION PRINCIPAL (declared): <name>` 是 naming 的**第三格输入**。
/// 在那之前模型从正文里推不出"这批话有一个主人"——human 轮与 agent 轮长得一样，于是它把
/// 用户自己的陈述挂到 agent 的锚上（现网 35/36 条 fact 的主语就是这样错的）。
/// 身份未知时写 `NONE`（**明确的"不知道"，不是省略**）：省掉这一行与"没有主人"在渲染上
/// 无法区分，模型会把沉默读成"没有人"；`NONE` 至少让它知道"这一格问过了、答案是不知道"。
/// 这一格只认声明值（`resolve_principal` 的读数），**昵称/主机名/账号名永远不许进来**。
pub fn a1_user(batch: &Batch, candidates: &[AnchorCandidate], declared: Option<&str>) -> String {
    let mut s = String::new();
    // 与候选锚同一个输入面：名字也要过 `prepare`（声明值可能来自环境变量，形状不受契约保护）。
    let principal = match declared.map(str::trim).filter(|v| !v.is_empty()) {
        Some(name) => prepare(name),
        None => "NONE".to_string(),
    };
    s.push_str(&format!("SESSION PRINCIPAL (declared): {principal}\n"));
    if !candidates.is_empty() {
        s.push_str("EXISTING CANDIDATES (naming hints only; do not copy ids into the output):\n");
        for c in candidates {
            s.push_str(&format!("- {} | {}", prepare(&c.kind), prepare(&c.canonical)));
            if !c.aliases.is_empty() {
                // 别名与 canonical 同一个输入面：零宽字符/围栏在这里同样要剪，
                // 否则"剪过的正文 + 没剪的别名"会让模型把同一个字面当成两个名字。
                let aliases: Vec<String> = c.aliases.iter().map(|a| prepare(a)).collect();
                s.push_str(&format!(" | aliases: {}", aliases.join(", ")));
            }
            s.push('\n');
        }
        s.push_str("When a turn talks about one of these, reuse its canonical name instead of inventing a variant.\n");
    }
    s.push_str("TURNS (ref in brackets, then role, then text):\n");
    for (reference, row) in a1_refs(batch) {
        // 摘要块是索引，不是命名来源：让模型看见它，只会把摘要里的名字当成原文证据。
        let text = prepare(&row.body);
        s.push_str(&format!("[{reference}] ({}) {text}\n", row.role));
    }
    s.push_str(
        "Turn refs are short batch-local ids (t1, t2, ...). Copy them verbatim into evidence and \
         turn_uid. Do not invent refs.\n",
    );
    s
}

/// A1 的批内字符预算：比 v3 多留一点给提及（一轮可能多个表面名）。
pub const fn a1_char_budget(rows: usize) -> usize {
    300 + rows * 4_500
}
