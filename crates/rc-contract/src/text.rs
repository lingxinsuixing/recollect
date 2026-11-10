//! 文本渲染原语：token 近似、本地时间、相对时间。
//!
//! 三个读者的**唯一家**：`rc-core` 的 20K 常驻简报与 `rc-agent` 的检索 brief 用同一个
//! `estimate_tokens`/`render_delta`，否则同一条记忆在两张面上会被算成不同长度、说成不同时间。
//! 时区**不由本模块决定**：`format_local_ms` 只收偏移量，固定 +08:00 的常量住调用方。

/// 保守 token 近似，**不是**模型 tokenizer：ASCII 按 4 字符≈1 token，非 ASCII（CJK 等）按
/// 1 字符≈1 token，`ascii/4 + non_ascii`，非空至少 1。它只会高估、不会低估，因此
/// "`token_estimate` ≤ budget" 是对真实 token 数的保守承诺。
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let mut ascii = 0usize;
    let mut non_ascii = 0usize;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    (ascii / 4 + non_ascii).max(1)
}

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// 过去时间说"N 分钟/小时/天前"，未来时间说"还有 N 分钟/小时/天"。
/// `then_ms <= 0` 视为"不知道"（载荷用 0 表示未填），绝不渲染成 1970 年的时间点。
pub fn render_delta(now_ms: i64, then_ms: i64) -> String {
    if then_ms <= 0 {
        return "未知".to_string();
    }
    let delta = now_ms.saturating_sub(then_ms);
    if delta >= 0 {
        if delta < MINUTE_MS {
            "刚刚".to_string()
        } else if delta < HOUR_MS {
            format!("{} 分钟前", delta / MINUTE_MS)
        } else if delta < DAY_MS {
            format!("{} 小时前", delta / HOUR_MS)
        } else {
            format!("{} 天前", delta / DAY_MS)
        }
    } else {
        let ahead = then_ms.saturating_sub(now_ms);
        if ahead < MINUTE_MS {
            "马上".to_string()
        } else if ahead < HOUR_MS {
            format!("还有 {} 分钟", ahead / MINUTE_MS)
        } else if ahead < DAY_MS {
            format!("还有 {} 小时", ahead / HOUR_MS)
        } else {
            format!("还有 {} 天", ahead / DAY_MS)
        }
    }
}

/// `YYYY-MM-DD HH:MM`，按给定偏移量渲染（分钟，东八区 = 480）。
pub fn format_local_ms(ms: i64, tz_offset_min: i32) -> String {
    let (y, m, d, hh, mm) = civil_parts(ms, tz_offset_min);
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// `YYYY-MM-DD`，按给定偏移量渲染。
pub fn format_local_date(ms: i64, tz_offset_min: i32) -> String {
    let (y, m, d, _, _) = civil_parts(ms, tz_offset_min);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `2023 年 11 月 15 日`，按给定偏移量渲染。给 LLM 面用：中文正文里不写 ISO 连字符。
pub fn format_cn_date(ms: i64, tz_offset_min: i32) -> String {
    let (y, m, d, _, _) = civil_parts(ms, tz_offset_min);
    format!("{y} 年 {m} 月 {d} 日")
}

/// `2023 年 11 月 15 日 6 点 13 分`，按给定偏移量渲染。给 LLM 面用。
pub fn format_cn_ms(ms: i64, tz_offset_min: i32) -> String {
    let (y, m, d, hh, mm) = civil_parts(ms, tz_offset_min);
    format!("{y} 年 {m} 月 {d} 日 {hh} 点 {mm} 分")
}

fn civil_parts(ms: i64, tz_offset_min: i32) -> (i64, u32, u32, i64, i64) {
    let local = ms.saturating_add(i64::from(tz_offset_min) * MINUTE_MS);
    let days = local.div_euclid(DAY_MS);
    let rem = local.rem_euclid(DAY_MS);
    let (y, m, d) = civil_from_days(days);
    (y, m, d, rem / HOUR_MS, (rem % HOUR_MS) / MINUTE_MS)
}

/// Howard Hinnant 的 `civil_from_days`：纯整数、无依赖，负年也成立。
/// `days` 是自 1970-01-01 起的天数；返回公历年月日。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_近似与_rc_core_同一把尺子() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens(&"a".repeat(400)), 100);
        assert_eq!(estimate_tokens(&"中".repeat(100)), 100);
        assert_eq!(estimate_tokens("abcd中文"), 3);
        assert_eq!(estimate_tokens("abcdefgh中"), 3);
    }

    #[test]
    fn 相对时间覆盖过去与未来_零点视为未知() {
        let now = 1_700_000_000_000;
        assert_eq!(render_delta(now, now - 10_000), "刚刚");
        assert_eq!(render_delta(now, now - 5 * MINUTE_MS), "5 分钟前");
        assert_eq!(render_delta(now, now - 3 * HOUR_MS), "3 小时前");
        assert_eq!(render_delta(now, now - 2 * DAY_MS), "2 天前");
        assert_eq!(render_delta(now, now + 2 * DAY_MS), "还有 2 天");
        assert_eq!(render_delta(now, now + 3 * HOUR_MS), "还有 3 小时");
        assert_eq!(render_delta(now, 0), "未知");
    }

    #[test]
    fn 本地日期按偏移量渲染_零点是_epoch() {
        assert_eq!(format_local_ms(0, 0), "1970-01-01 00:00");
        assert_eq!(format_local_ms(0, 480), "1970-01-01 08:00");
        assert_eq!(format_local_date(0, 0), "1970-01-01");
        // 1_700_000_000_000 ms = 2023-11-14T22:13:20Z；+08:00 ⇒ 2023-11-15 06:13
        assert_eq!(format_local_ms(1_700_000_000_000, 480), "2023-11-15 06:13");
        assert_eq!(format_local_date(1_700_000_000_000, 480), "2023-11-15");
        assert_eq!(format_cn_date(1_700_000_000_000, 480), "2023 年 11 月 15 日");
        assert_eq!(format_cn_ms(1_700_000_000_000, 480), "2023 年 11 月 15 日 6 点 13 分");
        assert_eq!(format_cn_date(0, 480), "1970 年 1 月 1 日");
        assert_eq!(format_cn_ms(0, 480), "1970 年 1 月 1 日 8 点 0 分");
    }

    #[test]
    fn 负毫秒与闰年边界不炸() {
        // -1 ms = 1969-12-31T23:59:59.999Z
        assert_eq!(format_local_ms(-1, 0), "1969-12-31 23:59");
        // 2024-02-29T00:00:00Z = 1709164800000
        assert_eq!(format_local_date(1_709_164_800_000, 0), "2024-02-29");
    }
}
