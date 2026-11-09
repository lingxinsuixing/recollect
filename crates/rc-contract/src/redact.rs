//! 入库时值级脱敏（依据：`design/2026-10-25-extraction-system-blueprint.md`
//! 第 31–32 行「入库前必须做密钥/口令脱敏」与硬规则 1「只做脱敏与结构归一」）。
//!
//! 只改**入库正文**：`turn_uid`/`body_fp` 仍由调用方按**原文**计算，所以脱敏不改变一条轮
//! 的身份，重复导入照旧幂等。命中片段替换成 [`REDACTED_CREDENTIAL`]，原文只经
//! [`RedactionEntry`] 交给调用方用来写**显式指定**的本地映射文件——本模块不写任何文件，
//! 也不把原文放进任何 `Debug`/`Display` 出口（守卫 12）。
//!
//! 判据是三支窄口径，刻意**不要**裸高熵/裸长 hex（实测误杀 80–95%，主力是路径、文件名、
//! 会话 UUID）：
//!
//! 1. 口令关键词 + 同行值：`密码|口令|pass(word|wd|phrase)|\bpass\s*=`（大小写不敏感），
//!    值须 8–64 个"非空白、非常见标点"的字符，且同时含字母与数字；
//! 2. 有固定前缀的密钥：词边界锚定的 `AKIA`（后接 ≥16 位大写/数字）、
//!    `ghp_`（后接 ≥20 位）、`xoxb-`（后接 ≥10 位）、`sk-`（后接 ≥10 位）；
//! 3. `scheme://user:pass@host` 的 `pass` 段（≥4 位且不全为数字）。

use std::fmt;

/// 命中片段的替换文本。零信息量：不带原文长度、不带键名。
pub const REDACTED_CREDENTIAL: &str = "<redacted:credential>";

/// 三支判据的出处。落进映射文件时给运维看"是哪一支命中的"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// 口令关键词 + 同行值。
    KeywordValue,
    /// `AKIA` / `ghp_` / `xoxb-` / `sk-` 这类有固定前缀的密钥。
    TokenPrefix,
    /// `scheme://user:pass@host` 里的口令段。
    UrlUserInfo,
}

impl CredentialKind {
    pub fn slug(self) -> &'static str {
        match self {
            CredentialKind::KeywordValue => "keyword_value",
            CredentialKind::TokenPrefix => "token_prefix",
            CredentialKind::UrlUserInfo => "url_userinfo",
        }
    }
}

/// 命中区间，**字节**下标、左闭右开：`&text[start..end]` 就是被替换掉的原文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CredentialSpan {
    pub start: usize,
    pub end: usize,
    pub kind: CredentialKind,
}

/// 脱敏后的正文 + 命中清单（计数从这里读，正文不回头）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedText {
    pub text: String,
    pub spans: Vec<CredentialSpan>,
}

/// 一条"替换前长什么样"的本地记录。**明文只在这里**，由调用方在显式给了映射路径时写出；
/// 默认路径不构造它（只计数）。
///
/// `original` 字段私有 + 手写 `Debug`：这类对象一旦被 `{:?}`/`{}` 打到日志或错误里，
/// 明文就绕过了守卫 12。唯一读法是 [`RedactionEntry::original`]。
#[derive(Clone, PartialEq, Eq)]
pub struct RedactionEntry {
    pub turn_uid: String,
    pub role: String,
    pub kind: &'static str,
    pub start: usize,
    pub end: usize,
    original: String,
}

impl RedactionEntry {
    pub fn new(
        turn_uid: impl Into<String>,
        role: impl Into<String>,
        span: CredentialSpan,
        original: impl Into<String>,
    ) -> Self {
        RedactionEntry {
            turn_uid: turn_uid.into(),
            role: role.into(),
            kind: span.kind.slug(),
            start: span.start,
            end: span.end,
            original: original.into(),
        }
    }

    /// 被替换掉的原文。只在"写映射文件"这一处调用。
    pub fn original(&self) -> &str {
        &self.original
    }
}

impl fmt::Debug for RedactionEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RedactionEntry")
            .field("turn_uid", &self.turn_uid)
            .field("role", &self.role)
            .field("kind", &self.kind)
            .field("start", &self.start)
            .field("end", &self.end)
            .field("original", &"<redacted>")
            .finish()
    }
}

/// 扫一遍正文并替换命中片段。没有命中时原样返回（不复制）。
pub fn redact_text(text: &str) -> RedactedText {
    let spans = scan(text);
    if spans.is_empty() {
        return RedactedText { text: text.to_owned(), spans };
    }
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for s in &spans {
        out.push_str(&text[cursor..s.start]);
        out.push_str(REDACTED_CREDENTIAL);
        cursor = s.end;
    }
    out.push_str(&text[cursor..]);
    RedactedText { text: out, spans }
}

/// 全部命中（已排序、已合并重叠）。对上层的判据只有这一份。
pub fn scan(text: &str) -> Vec<CredentialSpan> {
    let chars: Vec<char> = text.chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_ascii_lowercase()).collect();
    let mut offs: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
    offs.push(text.len());

    let mut hits: Vec<RawSpan> = Vec::new();
    scan_keywords(&chars, &lower, &mut hits);
    scan_token_prefixes(&chars, &mut hits);
    scan_url_userinfo(&chars, &mut hits);

    // 同一段文本可能被两支同时认出（`password = ghp_…`）：先按起点、再按长度收敛，
    // 重叠的并成一段——宁可多盖一个字符，也不留半截明文在后面。
    hits.sort_by(|a, b| a.start.cmp(&b.start).then(b.end.cmp(&a.end)));
    let mut merged: Vec<RawSpan> = Vec::with_capacity(hits.len());
    for h in hits {
        match merged.last_mut() {
            Some(prev) if h.start < prev.end => {
                prev.end = prev.end.max(h.end);
            }
            _ => merged.push(h),
        }
    }
    merged
        .into_iter()
        .map(|h| CredentialSpan {
            start: offs[h.start],
            end: offs[h.end],
            kind: h.kind,
        })
        .collect()
}

/// 字符下标空间的命中区间（最后一步才换算成字节）。
#[derive(Debug, Clone, Copy)]
struct RawSpan {
    start: usize,
    end: usize,
    kind: CredentialKind,
}

fn starts_with(chars: &[char], at: usize, pat: &str) -> bool {
    let mut k = at;
    for p in pat.chars() {
        match chars.get(k) {
            Some(c) if *c == p => k += 1,
            _ => return false,
        }
    }
    true
}

/// 值的字符集：排除空白与常见标点（`,.;:'"()[]{}<>/\|` 等），保留口令里常见的
/// `_-+!@#$%^&*`。`<` 不在集合里 ⇒ 已打码的 `<redacted:credential>` 不会被第二次命中。
fn is_value_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+' | '!' | '@' | '#' | '$' | '%' | '^' | '&' | '*')
}

/// 行内空白（含全角空格）。关键词与值必须**同行**：换行是句子的边界，不是分隔符。
fn is_inline_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\u{3000}')
}

fn scan_keywords(chars: &[char], lower: &[char], hits: &mut Vec<RawSpan>) {
    let n = chars.len();
    let mut i = 0;
    while i < n {
        let Some(klen) = keyword_at(chars, lower, i) else {
            i += 1;
            continue;
        };
        let mut k = i + klen;
        while chars.get(k).is_some_and(|c| is_inline_space(*c)) {
            k += 1;
        }
        // 关键词与值之间的分隔符。`是`/`为` 只服务中文口语（"密码是 …"、"口令为 …"）。
        if chars.get(k).is_some_and(|c| matches!(*c, '=' | '＝' | ':' | '：' | '是' | '为')) {
            k += 1;
            while chars.get(k).is_some_and(|c| is_inline_space(*c)) {
                k += 1;
            }
        }
        // 引号只是分隔符，不属于值。
        if chars
            .get(k)
            .is_some_and(|c| matches!(*c, '"' | '\'' | '`' | '“' | '‘' | '「' | '『'))
        {
            k += 1;
        }
        let start = k;
        let mut end = k;
        while chars.get(end).is_some_and(|c| is_value_char(*c)) {
            end += 1;
        }
        let value = &chars[start..end];
        let shaped = (8..=64).contains(&value.len())
            && value.iter().any(|c| c.is_ascii_alphabetic())
            && value.iter().any(|c| c.is_ascii_digit());
        if !shaped {
            // 值不成立就不是凭据：往前挪一格继续扫（关键词本身可能只是散文里的一个词）。
            i += 1;
            continue;
        }
        hits.push(RawSpan { start, end, kind: CredentialKind::KeywordValue });
        i = end;
    }
}

/// 关键词命中的长度（字符数，含 `\bpass\s*=` 那条里的 `=`）。
fn keyword_at(chars: &[char], lower: &[char], i: usize) -> Option<usize> {
    if chars[i] == '密' && chars.get(i + 1) == Some(&'码') {
        return Some(2);
    }
    if chars[i] == '口' && chars.get(i + 1) == Some(&'令') {
        return Some(2);
    }
    // 拉丁关键词要求词边界：`passwordless`、`bypass` 都不是关键词。
    if i > 0 && (chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_') {
        return None;
    }
    for kw in ["passphrase", "password", "passwd"] {
        if starts_with(lower, i, kw) {
            let end = i + kw.chars().count();
            if chars.get(end).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
                return None;
            }
            return Some(kw.chars().count());
        }
    }
    // `(?i)\bpass\s*=`：裸 `pass` 只在跟着 `=` 时才算口令关键词，散文里的 pass 一律放过。
    if starts_with(lower, i, "pass") {
        let mut k = i + 4;
        while chars.get(k).is_some_and(|c| is_inline_space(*c)) {
            k += 1;
        }
        if chars.get(k) == Some(&'=') {
            return Some(k - i + 1);
        }
    }
    None
}

fn scan_token_prefixes(chars: &[char], hits: &mut Vec<RawSpan>) {
    let n = chars.len();
    let mut i = 0;
    while i < n {
        // 前缀必须是词首：`task-…` 里的 `sk-` 不算。
        if i > 0 && (chars[i - 1].is_ascii_alphanumeric() || chars[i - 1] == '_') {
            i += 1;
            continue;
        }
        let found = if starts_with(chars, i, "AKIA") {
            let mut j = i + 4;
            while chars.get(j).is_some_and(|c| c.is_ascii_uppercase() || c.is_ascii_digit()) {
                j += 1;
            }
            (j - i - 4 >= 16).then_some(j)
        } else if starts_with(chars, i, "ghp_") {
            let mut j = i + 4;
            while chars.get(j).is_some_and(|c| c.is_ascii_alphanumeric()) {
                j += 1;
            }
            (j - i - 4 >= 20).then_some(j)
        } else if starts_with(chars, i, "xoxb-") {
            let mut j = i + 5;
            while chars.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '-') {
                j += 1;
            }
            (j - i - 5 >= 10).then_some(j)
        } else if starts_with(chars, i, "sk-") {
            let mut j = i + 3;
            while chars.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
                j += 1;
            }
            (j - i - 3 >= 10).then_some(j)
        } else {
            None
        };
        match found {
            Some(end) => {
                hits.push(RawSpan { start: i, end, kind: CredentialKind::TokenPrefix });
                i = end;
            }
            None => i += 1,
        }
    }
}

/// `scheme://user:pass@host`：只替换 `pass` 那一段，`user`/`host`/`scheme` 原样保留。
fn scan_url_userinfo(chars: &[char], hits: &mut Vec<RawSpan>) {
    let n = chars.len();
    let mut i = 0;
    while i + 3 <= n {
        if !(chars[i] == ':' && chars[i + 1] == '/' && chars[i + 2] == '/') {
            i += 1;
            continue;
        }
        let mut s = i;
        while s > 0 && (chars[s - 1].is_ascii_alphanumeric() || matches!(chars[s - 1], '+' | '-' | '.')) {
            s -= 1;
        }
        let scheme_ok = s < i
            && chars[s].is_ascii_alphabetic()
            && chars[s..i].iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
        if !scheme_ok {
            i += 1;
            continue;
        }
        // userinfo 到 `@` 为止；路径/查询/片段/引号/括号都意味着 "这不是一个带口令的 URL"。
        let mut j = i + 3;
        let mut colon = None;
        let mut at = None;
        while j < n {
            let c = chars[j];
            if c.is_whitespace()
                || matches!(c, '/' | '?' | '#' | '"' | '\'' | '`' | '(' | ')' | '[' | ']' | '{' | '}' | '<' | '>' | ',' | ';')
            {
                break;
            }
            if c == ':' && colon.is_none() {
                colon = Some(j);
            }
            if c == '@' {
                at = Some(j);
                break;
            }
            j += 1;
        }
        if let (Some(c), Some(a)) = (colon, at) {
            let pass = &chars[c + 1..a];
            // `user` 非空、`pass` ≥4 位且不全为数字（后者挡的是 `scheme://host:8080@…` 这类
            // 把端口当口令的误伤）。
            let shaped = a > i + 3
                && c > i + 3
                && pass.len() >= 4
                && pass.iter().any(|ch| !ch.is_ascii_digit());
            if shaped {
                hits.push(RawSpan { start: c + 1, end: a, kind: CredentialKind::UrlUserInfo });
                i = a;
                continue;
            }
        }
        i += 1;
    }
}
