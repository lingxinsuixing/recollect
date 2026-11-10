//! 脱敏（A47 节 4 末段）：**按"任何家的任何 body"通配，按形状而不是按内容判断**。
//! 依据：同一家同一路径的错误形状随时间变化；DeepSeek 的 401 回显 key 后缀、另一天返零字节；
//! MiniMax 业务码文案**可能含用户内容片段**。判据 N5 钉的就是"这个文件里不许出现厂家名字面"。
//!
//! 落点是唯一的：运维日志只有 `crate::ops_line` 一处，而它只经这里。原文**只进 stderr、不落文件**
//! （provider 原文会带出会话正文，落文件 = 第二条无守卫的正文下行路径，与 A48 备份面相乘）。

/// 原文进日志的封顶长度。超长不是"截了就好"：截断位置之后的内容不再被任何规则看过，
/// 所以**先剥敏感形状、再截长度**（`redact` 的函数体顺序就是这条纪律）。
pub const MAX_LOG_BYTES: usize = 240;

/// 判定"这是一个不透明长串"的长度门限。低于它的都留下——实测需要留在日志里可读的最长标识是
/// `max_completion_tokens`（21 字符），而实测需要剥掉的最短东西是 MiniMax 的裸 key（45 字符）。
/// 门限夹在两者之间 ⇒ 结构可读、凭据不可读，且这条界是**形状**的，不是词汇表的。
pub const LONG_TOKEN_MIN: usize = 24;

/// 会被**整值剥掉**的 JSON 键名名单。这一条补的是形状规则盖不住的那一族：各家会把请求正文
/// 逐字复述回错误体（实测 SenseNova/MiniMax 都复述过批次原文），而复述回来的正是用户内容，
/// 其中 CJK 段落里没有 ASCII 长串 ⇒ 规则①一条都剥不掉。
/// 名单是 **JSON 键名的形状**（不是某家的措辞），所以它不违反"不许按 provider 分支"。
pub const ECHOED_TEXT_KEYS: [&str; 6] = ["content", "text", "prompt", "input", "query", "messages"];

/// 四条通配规则（顺序有意义：②③要看见①已经打下来的 `<id>` 边界，④先于①跑）。
/// ④ `"content":"…"` 那一族的**值**整段换成 `<text>`：复述回来的用户原文从这里出去；
/// ① 连续 24 个以上的字母数字（含 `_` `-` `.`）⇒ 整串换成 `<id>`：裸 key、32 位十六进制的
///    `x-request-id`、带时间戳前缀的那种（`2026101503523286932075NDLB5rLX`）三种形状一起盖住；
/// ② `****` 后面紧跟的一串 ⇒ 一起吃掉：DeepSeek 的 401 回显的是**后 4 位**，只留 `****` 才不剩尾巴；
/// ③ `@` 后面紧跟的一串 ⇒ 换成 `@<masked>`：MiniMax 的 `content：@superman` 那一族。
pub fn redact(body: &str) -> String {
    let masked = mask_echoed_values(body);
    let mut out = String::with_capacity(masked.len().min(MAX_LOG_BYTES));
    let mut run = String::new();
    // 先把长串那一条走一遍（规则①），再补另外两条（②③）。分两趟是为了让②看得见①的产物。
    for c in masked.chars() {
        if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.') {
            run.push(c);
            continue;
        }
        push_run(&mut out, &mut run);
        out.push(c);
    }
    push_run(&mut out, &mut run);

    let mut stripped = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '*' {
            while chars.peek().is_some_and(|ch| *ch == '*') {
                chars.next();
            }
            // 规则②：`****` 之后紧跟的字母数字一并吃掉（那串就是 key 的尾巴）。
            while chars.peek().is_some_and(|ch| ch.is_ascii_alphanumeric()) {
                chars.next();
            }
            stripped.push_str("****");
            continue;
        }
        if c == '@' {
            // 邮箱的本地部分（`support@example.com` 的前半）也一起走：这一条吃的是整个标识符形状。
            // `stripped` 结尾是 `<id>` 时不许再退——那是规则①已经落下的标记，不是正文的一部分。
            if !stripped.ends_with("<id>") {
                while stripped
                    .ends_with(|ch: char| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
                {
                    stripped.pop();
                }
            }
            while chars
                .peek()
                .is_some_and(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
            {
                chars.next();
            }
            stripped.push_str("@<masked>");
            continue;
        }
        stripped.push(c);
    }

    // 截断放在最后：先剥形状、后截长度，否则被截掉的那一段永远没被看过。
    let mut cut: String = stripped.chars().take(MAX_LOG_BYTES).collect();
    if cut.chars().count() < stripped.chars().count() {
        cut.push_str("...[truncated]");
    }
    cut
}

/// 规则①的落点：一个长串到边界了，够长就换 `<id>`，不够长原样留下。
fn push_run(out: &mut String, run: &mut String) {
    if run.chars().count() >= LONG_TOKEN_MIN {
        out.push_str("<id>");
    } else {
        out.push_str(run);
    }
    run.clear();
}

/// 规则④的实现：扫 `"名单键"` 后（允许空白）紧跟 `:`、再跟一个字符串值的情形，把**那一段值**
/// 换成 `<text>`。已知覆盖边界（写清楚而不是假装没有）：值本身是对象或数组时这一版不剥外壳，
/// 但数组里每一个字符串值都会被下一轮扫到（`messages` 那一族正是这样出去的）；带转义引号的值
/// 由 `find_value_end` 跳过 `\\"` 处理。要更强的保证就得引一个 JSON 解析器进日志路径，
/// 而日志路径上的解析器就是下一个 panic 点（节 4 那条"解析器不许在这里 panic"）。
fn mask_echoed_values(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    loop {
        // 名单里最早出现的那一个键。
        let hit = ECHOED_TEXT_KEYS
            .iter()
            .filter_map(|k| rest.find(&format!("\"{k}\"")).map(|at| (at, k.len())))
            .min();
        let (at, key_len) = match hit {
            Some(x) => x,
            None => {
                out.push_str(rest);
                return out;
            }
        };
        let after_key = at + key_len + 2;
        let tail = &rest[after_key..];
        let ws = |s: &str| s.chars().all(|c| c == ' ' || c == '\t');
        let colon = match tail.find(':') {
            Some(0) => 0,
            Some(n) if ws(&tail[..n]) => n,
            // 键名后面不是 `:`（比如它出现在正文里）⇒ 只推进一个字符，重新找。
            _ => {
                out.push_str(&rest[..at + 1]);
                rest = &rest[at + 1..];
                continue;
            }
        };
        let open = match tail[colon + 1..].find('"') {
            Some(o) if ws(&tail[colon + 1..colon + 1 + o]) => colon + 1 + o,
            // 值不是字符串（数字/布尔/对象/数组）⇒ 不剥这一格，从键名之后继续扫里面的字符串值。
            _ => {
                out.push_str(&rest[..after_key]);
                rest = &rest[after_key..];
                continue;
            }
        };
        let close = find_value_end(&tail[open + 1..]);
        // 前缀的索引在 `rest` 上、后缀的索引在 `tail` 上：混用就是把窗口起点算错一次
        // （我这版第一遍就错了，用例 `名单里那三个键...` 当场红）。
        out.push_str(&rest[..after_key + open]);
        out.push_str("<text>");
        rest = &tail[(open + 2 + close).min(tail.len())..];
    }
}

/// 跳过转义找一个字符串值的**结束引号**，返回它在 `s` 里的下标；整个剩余都没闭合时返回 `s.len()`
/// （那一格是残缺 JSON，剥到末尾正是想要的行为）。
fn find_value_end(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return i,
            _ => i += 1,
        }
    }
    b.len()
}
